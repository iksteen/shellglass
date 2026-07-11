# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

shellglass mirrors an interactive command (run in a PTY, the `script(1)` model) as live HTML in a browser: terminal state lives in a long-lived `vt100` parser and is pushed to the browser over SSE (no polling, no per-tick subprocess). Read `README.md` for the user-facing model — this file covers the internals.

## Commands

```sh
cargo build --release            # full binary + per-mode bins at ./target/release/
                                 # (shellglass, shellglass-{serve,push,hub,gen-key,print-id})
cargo check --no-default-features --features hub   # modes are cargo features —
                                 # hub/serve/push subset builds must stay warning-free (CI matrix)
cargo test --workspace           # unit tests incl. vendored vt100 suite (CI runs with --locked)
cargo test id_is_deterministic   # a single test by name
cargo fmt --all --check          # CI gate
cargo clippy --workspace --all-targets -- -D warnings   # CI gate (pedantic-clean; keep it that way)

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

**Input backend** — `pty.rs` (started from `main.rs`; in push mode only after the first successful hub registration, so a down hub retries before the command runs) produces a `watch::Receiver<Arc<Frame>>` of structured screen snapshots (`Frame::Screen(Grid)`). It runs one command in a PTY (`script(1)` model); one parser, one screen. A single `screen` thread owns raw mode + stdout + parser so hub-connection notices can pause/repaint cleanly, and snapshots at a 30fps cap (`MIN_FRAME`), holding frames during a synchronized
update (DEC 2026, `SyncGate`) with a 1s deadline so viewers never see a torn
mid-redraw snapshot and a stuck BSU can't freeze the mirror. The command defaults to `$SHELL`; `SIGWINCH` reflows both the PTY and the parser. Unix only.

The `screen` thread also runs the inline-image side channel (`images.rs`): raw PTY bytes go through an `Interceptor` that pulls out kitty/iTerm2/sixel sequences (vt100 drops them) as `Segment::Image` and feeds everything else to the parser as `Segment::Pass`. A placed image is tracked with **per-cell image tags** — the vendored vt100's `place_image` stamps `(id, row_off, col_off)` into every covered cell, and the tag dies with the cell's contents (set/clear), so vt100's own scroll/reflow/erase manage the image's lifetime exactly like a cell-based sixel terminal erases an image. Each frame `resolve_images` reconstructs the top-left from the most top-left surviving tag (its stored offset makes any survivor exact; a bottom-row survivor yields a negative row → viewer clips) and evicts once no covered cell is left. The cursor is left on the image's last row (where a sixel-scrolling terminal leaves it). Placements ride the frame as `Grid::images`.

**Three serving modes** (dispatched in `cli.rs`; every binary — the full multi-call `shellglass` and the per-mode ones in `src/bin/` — wraps the same clap actions there; modes are cargo features):
- Standalone (`server.rs`) — local axum server: `GET /` page, `GET /events` SSE deltas, `GET /viewer.js` the baked renderer.
- Client (`client.rs`) — keeps the previously-sent frame and streams `diff.rs` wire messages to a remote hub over one `/push` **WebSocket**: a `RegisterBody` (CSS/fonts/render-config) as the first message, then a full picture on each (re)connect, then only deltas (a resize is a layout change, which `encode_delta` turns into a full). Pings for liveness (a run of unanswered pongs, or a send stalled past a timeout, ⇒ dead ⇒ reconnect — so a black-holed hub is caught in seconds, not the kernel's ~15-min TCP timeout). Re-registers + reconnects on drop.
- Hub (`hub.rs`, `hub` subcommand) — renders nothing and re-diffs nothing; the `/push` WebSocket runs a `AwaitingRegister → Streaming` state machine (first message = register, rest = wire messages), authorized once at the upgrade. Stores each client's pushed CSS/fonts/render-config, applies each pushed wire message to the session's full matrix (`diff::Live::publish_wire`) and forwards the bytes to viewers verbatim; serves viewers at `/s/<id>`. On SIGTERM it Closes every push WebSocket (see `main`'s graceful path) so pushers reconnect promptly instead of black-holing.

**Key layers:**
- `model.rs` — parser-agnostic in-memory IR (`StyledCell`, `Grid`, `Frame`); `Color` and `ImagePlacement` carry serde (embedded in the wire — cell styles, and the full frame's `i` list). Nothing here depends on `vt100`.
- `images.rs` — inline-image interceptor. Scans raw PTY bytes for kitty (`_G` APC), iTerm2 (OSC 1337, single-shot + multipart), and sixel (`ESC P … q`) sequences, reassembling ones split across reads, and decodes/normalizes each to a browser-native image (sixel and raw kitty RGB/RGBA → PNG; iTerm2 forwards PNG/JPEG/GIF/WebP; kitty file/temp/shm transports read the referenced bytes read-only). Which protocols to intercept is a startup capability handshake (`probe_caps` in `pty.rs`: kitty query, sixel via Primary DA, iTerm2 via a `TERM_PROGRAM` allowlist) — **mirror fidelity is a hard rule**: a protocol the terminal doesn't render passes straight through to vt100, so the web shows exactly what the local screen shows, never more, never less.
- `parse.rs` — `grid_from_screen` extracts a `Grid` from a live `vt100::Screen`.
- `crates/vt100/` — the terminal emulator, vendored from doy/vt100-rust 0.16.2 (upstream
  unmaintained) as a workspace member, with upstream's full fixture test suite running in
  CI. Parser-level fidelity gaps are fixed HERE, not with pre-parser byte shims; every
  local patch is listed in the crate's `Cargo.toml` provenance header (keep it
  current — it is the authoritative patch list) and lands as its own commit so the
  diff against upstream stays reviewable. Planned parser/fidelity work is ordered in
  `docs/roadmap.md` — consult it before starting enhancement work.
- `diff.rs` — the whole wire layer. `Live`: diff-once, broadcast-to-all — the standalone server publishes `Frame`s (delta encoded here, once), the hub publishes the client's already-encoded messages (`publish_wire`: applied to the stored matrix, forwarded verbatim). **Connecting is lock-free** (`/s/<id>/events` is public, so a connect flood must not stall the publisher): deltas are broadcast with a monotonic seq, the current state lives in an `ArcSwap` snapshot with a memoized full encode, and viewers subscribe-then-snapshot, skipping deltas the snapshot covers; each SSE event carries its seq as the `id:` line. Publishers store-before-send — that ordering is the correctness argument. Deltas are **per-line** minimal changed spans as positional tuples `[r, l, text, style?]`; block text merges consecutive single-codepoint glyphs into strings (one cell per codepoint, `0` = blank, `["…"]` = one multi-codepoint-grapheme cell) and styles ride as `[start, len, {attrs}]` runs with `1`-flags (`u` carries the underline-*style* number 1–5 — kitty's `4:n` values, so `1` doubles as the legacy flag for old decoders — with `s` strikethrough and `k` underline color alongside). Rectangles were measured and removed (~5–6× win came from RLE+string-merge; merging lines never paid — see `zz_measure_wire_cost`). Layout (cols/row-count) change ⇒ full frame. Messages carry **no type tag**: each type owns one payload key (`d` full, `r` diff, `c` cell, `l` line, `b` banner, `v` version) with the cursor a separate tri-state `p`, and decode/`apply` dispatch on which key is present (`t` is the window *title*, riding only full/diff shapes — never the `c` form, whose envelope old viewers spread into cell objects where `t` is cell text) — **`c` first**, because the single-cell form flattens its style letters (`f,g,b,d,i,u,s,o,k,n,a,w`) into the envelope and `b`/`d`/`w` there must not read as banner/full/wide. Inline images ride the **full** frame only, under key `i` (a list of `ImagePlacement`); any change to the image set forces a full (images are rare and usually empty, so `encode_delta` just compares two empty vecs) — a new/moved/gone image can't ride a per-line diff.
- `viewer/viewer.ts` — **the** renderer (TypeScript); nothing is painted server-side. Applies full/diff/banner messages to an in-memory cell grid and re-renders dirty rows: run coalescing (blank cells with no visible ink ride the open run), absolute `left:{col}ch` positioning, SVG symbol cells (fill glyphs stretch via `textLength`, solid-line runs merge into one span), palette/dim/reverse math. **Paint is decoupled from apply**: messages update the grid synchronously, one coalesced rAF flush does the DOM/canvas work, and an adaptive shaper paces flushes by measured frame cost (EWMA, ≤`TARGET_LOAD` of wall-clock) — always the latest state, intermediate frames dropped, like the server's `MIN_FRAME`. Under sustained near-full-screen change (cmatrix-class) it flips to **storm mode**: the picture moves to the cell-exact canvas overlay (no layout/style cost, ~full server fps) while rows stay as transparent single-text-node *ghost text* so select/copy keep working through the `pointer-events:none` canvas (ghost sync patches text nodes IN PLACE via `replaceData`, preserving node identity and Range boundaries; **the whole picture — canvas and ghost — freezes from pointerdown and while a selection is live** (`pictureHeld`), so what you see, what you highlighted, and what Ctrl-C copies are the same thing; every repaint path — flush, blink timer, hover, cursor travel — respects the hold, and release catches both layers up in one step; default-bg canvas fills are skipped so the highlight shows through); a watchdog drops back to DOM 1.2s after the last stormy flush. The nav's **canvas toggle** (`shellglass-render`) pins storm on as a first-class renderer and is ON by default (`?render=dom` opts a fresh browser out); the **storm toggle** (`shellglass-storm`, hidden while canvas mode is on) gates the DOM-mode auto-escalation and is OFF by default (`?storm=on` opts in). The two renderers answer to **different references**: DOM mode is CSS-native (real anchors, native selection/find, `text-decoration`), while canvas mode's ground truth is *terminal behavior* (kitty — check its source for placement questions): ink seated inside the cell box, decorations clamped into the cell and drawn per cell (through spaces), DECSCUSR cursor shapes, images drawn under later text. `viewer/verify.py` (headless-Firefox screenshot rig) checks both: DOM/canvas centroid comparison for the fitting case plus terminal-semantics self-checks; canvas-vs-DOM divergences that are terminal-correct are documented in its output, not gated away. The default template's footer shows live stats (`#sg-stats`: throughput · fps · shaper cap · `canvas`/`storm` tag). Inline images (the full frame's `i` list) render as absolutely-positioned `<img>` overlays at `top:calc(r·lh)` / `left:{c}ch` over an `overflow:hidden` `#screen`, so a placement with a negative row clips against the top edge instead of overflowing. Pages ship an empty `#screen`; the first SSE event after the version hello is always a full frame, so the initial paint lands one round-trip in. `build.rs` compiles it (local `tsc`, else the committed `viewer/dist/viewer.js`), bakes it in via `include_str!`, and both servers serve it at `/viewer.js`.
- `render.rs` — page assembly only (no cell rendering): fills the page template, builds `@font-face` CSS and the render-config JSON handed to the browser, and carries the baked `viewer.js`/favicon assets + the `viewer_tag()` content hash.
- `fonts.rs` — `symbol_map` codepoint→family resolution + locating font files via `fontdb` (extracts a single face from `.ttc`) so viewers render glyphs with no local install. `fc-match` is consulted only to resolve a CSS generic like `monospace`.
- `config.rs` — TOML config (`default_font` stack, `symbol_map`, `template`). The built-in template carries an in-page CRT-effect toggle (off by default, localStorage-persisted) — there is no `theme` option.
- `proto.rs` — client/hub wire contract.

## Capability model (proto.rs)

The **secret key** is the write capability. The **session id** = `hex(argon2id(key))` with a fixed salt (`SALT`) is the read capability that goes in the view URL — one-way, so viewers can't recover the key. The hub screens pushes against a pre-registered `--allow` set of ids (`authorize` in `hub.rs`, at the `/push` upgrade — one argon2 per connection, semaphore-capped + fail2ban-logged on rejection); it never sees secrets. `session_id` is intentionally memory-hard — call it once per connection, never per frame. The salt's version suffix is **also the protocol-skew guard**: bump it on a breaking change to the **wire messages** (`diff.rs`) so a mismatched client/hub pair fails loudly at the `/push` upgrade (they derive different ids from the same key) instead of silently dropping frames. A new/renamed *endpoint* needs no bump (an old client hits a gone route — a loud 404, not a silent misread), which is why the WebSocket migration kept `v4`.

Push transport: one `/push` WebSocket per session. WS self-frames, so there's no length-prefix layer — one WS **Text** message = one wire message (register JSON, then `diff.rs` wire strings), capped at `proto::MAX_WS_MESSAGE`. Client ↔ hub liveness rides on WS ping/pong (axum auto-pongs; the client runs the timeout).

## Conventions

- Deliberate simplifications are marked with `ponytail:` comments naming the ceiling and upgrade path — respect them; don't "fix" a documented shortcut without reason.
- Non-SSE responses are compressed per-route; the SSE stream is never compressed (buffering would defeat realtime push).
- Errors from the input backend surface as an in-page `render::banner`, not a failed HTTP request.
- Shelved explorations are kept on `exp/<name>` branches pushed to origin (not deleted), so the code and the findings stay retrievable for future reference. Existing ones: `exp/procedural-glyph-geometry` — kitty-style procedural SVG geometry for box-drawing/block glyphs; shelved because the Nerd Font the default config auto-exports hints straight lines better than stretched-SVG rects can on a fractional device-pixel grid (see the branch's commit for the full reasoning).
