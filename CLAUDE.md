# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

shellglass mirrors an interactive command (run in a PTY, the `script(1)` model) as live HTML in a browser: terminal state lives in a long-lived `vt100` parser and is pushed to the browser over SSE (no polling, no per-tick subprocess). Read `README.md` for the user-facing model — this file covers the internals.

## Commands

```sh
cargo build --release            # binary at ./target/release/shellglass
cargo test                       # unit tests (CI runs with --locked)
cargo test id_is_deterministic   # a single test by name
cargo fmt --check                # CI gate
cargo clippy --all-targets -- -D warnings   # CI gate (pedantic-clean; keep it that way)

cd viewer && npm ci              # once: browser-renderer toolchain (TypeScript)
npx tsc --noEmit                 # type-check viewer.ts (CI gate)
node --test                      # viewer unit tests (CI gate)
npm run build                    # regenerate viewer/dist/viewer.js — COMMIT IT after
                                 # editing viewer.ts (CI fails if dist drifts)
```

`cargo build` works without Node: `build.rs` compiles `viewer/viewer.ts` with the local `tsc` when `viewer/node_modules` exists, else it bakes the committed `viewer/dist/viewer.js`.

CI (`.github/workflows/ci.yml`) runs fmt + clippy + tests plus the viewer job (type-check, tests, dist-freshness) on push/PR. Tagging `v*` triggers Docker Hub multi-arch and GitHub Release binary builds (both consume the committed `dist/`, no Node).

## Architecture

The core is one streaming pipeline shared by every mode:

```
PTY output bytes → vt100::Parser → model::Grid (StyledCell cells)
  → watch::Receiver<Arc<Frame>> → diff::Live (rectangle deltas, encoded once)
  → SSE → viewer.js renders cells → HTML in the browser
```

**Input backend** — `pty.rs` (`render_setup` in `main.rs`) produces a `watch::Receiver<Arc<Frame>>` of structured screen snapshots (`Frame::Screen(Grid)`). It runs one command in a PTY (`script(1)` model); one parser, one screen. A single `screen` thread owns raw mode + stdout + parser so hub-connection notices can pause/repaint cleanly, and snapshots at a 30fps cap (`MIN_FRAME`). The command defaults to `$SHELL`; `SIGWINCH` reflows both the PTY and the parser. Unix only.

**Three serving modes** (all in `main.rs`):
- Standalone (`server.rs`) — local axum server: `GET /` page, `GET /events` SSE deltas, `GET /viewer.js` the baked renderer.
- Client (`client.rs`) — streams full JSON `Frame`s to a remote hub over one persistent `/stream` POST (avoids per-frame HTTP round-trips); re-registers + reconnects on drop. (Diffing this link is a marked follow-up; the fan-out win is on the viewer SSE.)
- Hub (`hub.rs`, `hub` subcommand) — renders nothing; stores each client's pushed CSS/fonts/render-config, decodes its pushed frames into a per-session `diff::Live`, and serves viewers at `/s/<id>`.

**Key layers:**
- `model.rs` — parser-agnostic IR (`StyledCell`, `Grid`, `Frame`) doubling as the wire format: compact serde (single-letter keys, defaults omitted — a blank cell is `{}`). Nothing here depends on `vt100`.
- `parse.rs` — `grid_from_screen` extracts a `Grid` from a live `vt100::Screen`.
- `diff.rs` — `Live`: diff-once, broadcast-to-all. Computes each frame's delta from the previous **once** and broadcasts one pre-encoded message to every viewer; a connect atomically snapshots a full frame + subscribes; a lagged viewer resyncs with a fresh full. Deltas are per-row minimal changed cell-index spans, vertically merged into rectangles; cells ride columnar (dense text array, blank = `0`, sparse index→style map). Layout (cols/row-count) change ⇒ full frame.
- `viewer/viewer.ts` — the live browser renderer (TypeScript): applies full/diff/banner messages to an in-memory cell grid and re-renders dirty rows. **It must mirror `render.rs` byte-for-byte** (run coalescing, absolute `left:{col}ch` positioning, SVG symbol cells, palette/dim/reverse math) — nothing enforces this at build time, so change them together. `build.rs` compiles it (local `tsc`, else the committed `viewer/dist/viewer.js`), bakes it in via `include_str!`, and both servers serve it at `/viewer.js`.
- `render.rs` — the Rust reference renderer (`render_fragment`, kept in lockstep with `viewer.ts`), used for the standalone `GET /` initial paint; also builds `@font-face` CSS, the render-config JSON handed to the browser, and fills the page template.
- `fonts.rs` — `symbol_map` codepoint→family resolution + locating font files via `fontdb` (extracts a single face from `.ttc`) so viewers render glyphs with no local install. `fc-match` is consulted only to resolve a CSS generic like `monospace`.
- `config.rs` — TOML config (`default_font` stack, `symbol_map`, `theme`/`template`).
- `proto.rs` — client/hub wire contract.

## Capability model (proto.rs)

The **secret key** is the write capability. The **session id** = `hex(argon2id(key))` with a fixed salt (`SALT`) is the read capability that goes in the view URL — one-way, so viewers can't recover the key. The hub screens pushes against a pre-registered `--allow` set of ids (`authorize` in `hub.rs`); it never sees secrets. `session_id` is intentionally memory-hard — call it once per connection, never per frame.

Wire framing for the streaming push: `frame_encode`/`frame_drain` (`[u32 BE length][payload]`), capped at `MAX_FRAME`.

## Conventions

- Deliberate simplifications are marked with `ponytail:` comments naming the ceiling and upgrade path — respect them; don't "fix" a documented shortcut without reason.
- Non-SSE responses are compressed per-route; the SSE stream is never compressed (buffering would defeat realtime push).
- Errors from the input backend surface as an in-page `render::banner`, not a failed HTTP request.
