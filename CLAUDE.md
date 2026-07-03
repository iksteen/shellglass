# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

shellglass mirrors a terminal session as live HTML in a browser: terminal state lives in a long-lived `vt100` parser and is pushed to the browser over SSE (no polling, no per-tick subprocess). Read `README.md` for the user-facing model — this file covers the internals.

## Commands

```sh
cargo build --release            # binary at ./target/release/shellglass
cargo test                       # unit tests (CI runs with --locked)
cargo test id_is_deterministic   # a single test by name
cargo fmt --check                # CI gate
cargo clippy --all-targets -- -D warnings   # CI gate (pedantic-clean; keep it that way)
```

CI (`.github/workflows/ci.yml`) runs fmt + clippy + tests on push/PR. Tagging `v*` triggers Docker Hub multi-arch and GitHub Release binary builds.

## Architecture

The core is one rendering pipeline shared by every mode:

```
input backend → per-pane vt100::Parser → model::Window (StyledCell grid)
  → render::render_fragment (HTML) → watch::Receiver<String> → SSE to browser
```

**Two input backends**, both producing a `watch::Receiver<String>` of the latest `#screen` HTML fragment (see `start_backend` in `main.rs`):
- `live.rs` — tmux `-C` control mode. Seeds each pane parser from `capture-pane` (`tmux.rs`), then feeds the incremental `%output` byte stream. Runs on a dedicated OS thread with a 30fps cap (`MIN_FRAME`). Any structural event (`%layout-change`, window switch, …) triggers a full re-capture rather than parsing tmux layout strings.
- `pty.rs` — `--exec` runs one command in a PTY (`script(1)` model); one parser, one pane. A single `screen` thread owns raw mode + stdout + parser so hub-connection notices can pause/repaint cleanly. Unix only.

**Three serving modes** (all in `main.rs`):
- Standalone (`server.rs`) — local axum server, `GET /` page + `GET /events` SSE.
- Client (`client.rs`) — renders locally, streams frames to a remote hub over one persistent `/stream` POST (avoids per-frame HTTP round-trips); re-registers + reconnects on drop.
- Hub (`hub.rs`, `hub` subcommand) — renders nothing; stores each client's pushed CSS/fonts/frames and re-serves them at `/s/<id>`.

**Key layers:**
- `model.rs` — parser-agnostic IR (`StyledCell`, `Grid`, `Pane`, `Window`). Nothing here depends on `vt100`, so the parse layer is swappable. This is the boundary both backends converge on.
- `parse.rs` — drives `vt100` to turn `capture-pane -e` output and `%output` bytes into a `Grid`.
- `render.rs` — `Grid` → HTML. Panes absolutely positioned in cell units; adjacent same-style cells coalesced into one `<span>`. Also builds `@font-face` CSS and fills the page template.
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
