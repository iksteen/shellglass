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

**Input backend** — `pty.rs` (started from `main.rs`; in push mode only after the first successful hub registration, so a down hub retries before the command runs) produces a `watch::Receiver<Arc<Frame>>` of structured screen snapshots (`Frame::Screen(Grid)`). It runs one command in a PTY (`script(1)` model); one parser, one screen. A single `screen` thread owns raw mode + stdout + parser so hub-connection notices can pause/repaint cleanly, and snapshots at a 30fps cap (`MIN_FRAME`). The command defaults to `$SHELL`; `SIGWINCH` reflows both the PTY and the parser. Unix only.

**Three serving modes** (all in `main.rs`):
- Standalone (`server.rs`) — local axum server: `GET /` page, `GET /events` SSE deltas, `GET /viewer.js` the baked renderer.
- Client (`client.rs`) — keeps the previously-sent frame and streams `diff.rs` wire messages to a remote hub over one persistent `/stream` POST: a full picture on each (re)connect, then only deltas (a resize is a layout change, which `encode_delta` turns into a full). Re-registers + reconnects on drop.
- Hub (`hub.rs`, `hub` subcommand) — renders nothing and re-diffs nothing; stores each client's pushed CSS/fonts/render-config, applies each pushed wire message to the session's full matrix (`diff::Live::publish_wire`) and forwards the bytes to viewers verbatim; serves viewers at `/s/<id>`.

**Key layers:**
- `model.rs` — parser-agnostic in-memory IR (`StyledCell`, `Grid`, `Frame`); only `Color` carries serde (embedded in the wire's cell styles). Nothing here depends on `vt100`.
- `parse.rs` — `grid_from_screen` extracts a `Grid` from a live `vt100::Screen`.
- `diff.rs` — the whole wire layer. `Live`: diff-once, broadcast-to-all — the standalone server publishes `Frame`s (delta encoded here, once), the hub publishes the client's already-encoded messages (`publish_wire`: applied to the stored matrix, forwarded verbatim). **Connecting is lock-free** (`/s/<id>/events` is public, so a connect flood must not stall the publisher): deltas are broadcast with a monotonic seq, the current state lives in an `ArcSwap` snapshot with a memoized full encode, and viewers subscribe-then-snapshot, skipping deltas the snapshot covers; each SSE event carries its seq as the `id:` line. Publishers store-before-send — that ordering is the correctness argument. Deltas are **per-line** minimal changed spans as positional tuples `[r, l, text, style?]`; block text merges consecutive single-codepoint glyphs into strings (one cell per codepoint, `0` = blank, `["…"]` = one multi-codepoint-grapheme cell) and styles ride as `[start, len, {attrs}]` runs with `1`-flags. Rectangles were measured and removed (~5–6× win came from RLE+string-merge; merging lines never paid — see `zz_measure_wire_cost`). Layout (cols/row-count) change ⇒ full frame. Messages carry **no `"t"` tag**: each type owns one payload key (`d` full, `r` diff, `c` cell, `l` line, `b` banner, `v` version) with the cursor a separate tri-state `p`, and decode/`apply` dispatch on which key is present — **`c` first**, because the single-cell form flattens its style letters (`f,g,b,d,i,u,n,w`) into the envelope and `b`/`d`/`w` there must not read as banner/full/wide.
- `viewer/viewer.ts` — **the** renderer (TypeScript); nothing is painted server-side. Applies full/diff/banner messages to an in-memory cell grid and re-renders dirty rows: run coalescing, absolute `left:{col}ch` positioning, SVG symbol cells (fill glyphs stretch via `textLength`, solid-line runs merge into one span), palette/dim/reverse math. Pages ship an empty `#screen`; the first SSE event after the version hello is always a full frame, so the initial paint lands one round-trip in. `build.rs` compiles it (local `tsc`, else the committed `viewer/dist/viewer.js`), bakes it in via `include_str!`, and both servers serve it at `/viewer.js`.
- `render.rs` — page assembly only (no cell rendering): fills the page template, builds `@font-face` CSS and the render-config JSON handed to the browser, and carries the baked `viewer.js`/favicon assets + the `viewer_tag()` content hash.
- `fonts.rs` — `symbol_map` codepoint→family resolution + locating font files via `fontdb` (extracts a single face from `.ttc`) so viewers render glyphs with no local install. `fc-match` is consulted only to resolve a CSS generic like `monospace`.
- `config.rs` — TOML config (`default_font` stack, `symbol_map`, `template`). The built-in template carries an in-page CRT-effect toggle (off by default, localStorage-persisted) — there is no `theme` option.
- `proto.rs` — client/hub wire contract.

## Capability model (proto.rs)

The **secret key** is the write capability. The **session id** = `hex(argon2id(key))` with a fixed salt (`SALT`) is the read capability that goes in the view URL — one-way, so viewers can't recover the key. The hub screens pushes against a pre-registered `--allow` set of ids (`authorize` in `hub.rs`); it never sees secrets. `session_id` is intentionally memory-hard — call it once per connection, never per frame. The salt's version suffix is **also the protocol-skew guard**: bump it on every breaking wire change so a mismatched client/hub pair fails loudly at `/register` (they derive different ids from the same key) instead of silently dropping frames.

Wire framing for the streaming push: `frame_encode`/`frame_drain` (`[u32 BE length][payload]`), capped at `MAX_FRAME`.

## Conventions

- Deliberate simplifications are marked with `ponytail:` comments naming the ceiling and upgrade path — respect them; don't "fix" a documented shortcut without reason.
- Non-SSE responses are compressed per-route; the SSE stream is never compressed (buffering would defeat realtime push).
- Errors from the input backend surface as an in-page `render::banner`, not a failed HTTP request.
- Shelved explorations are kept on `exp/<name>` branches pushed to origin (not deleted), so the code and the findings stay retrievable for future reference. Existing ones: `exp/procedural-glyph-geometry` — kitty-style procedural SVG geometry for box-drawing/block glyphs; shelved because the Nerd Font the default config auto-exports hints straight lines better than stretched-SVG rects can on a fractional device-pixel grid (see the branch's commit for the full reasoning).
