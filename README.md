# tmuxsnitch

Mirror a **tmux** window's full pane layout as live **HTML** in your browser, with
Kitty-style `symbol_map` font overrides (map Unicode codepoint ranges to specific
fonts, e.g. Nerd Font glyph ranges).

Snapshot mode: the server re-captures the target window on an interval and the page
polls for the fresh layout. The rendering pipeline is layered so the input can later
be swapped for tmux control mode (true live) without touching the renderer.

## How it works

```
tmux (list-panes geometry + capture-pane -e per pane)
  → vt100 terminal model  → parser-agnostic StyledCell grid
  → symbol_map font resolution
  → HTML (absolute-positioned panes, coalesced <span> runs)
  → axum:  GET /  (page + JS poller)   GET /snapshot  (fresh fragment)
```

## Usage

```sh
cargo run -- --target demo --config config.toml --bind 127.0.0.1:8080 --interval 500
```

- `--target`  tmux target (`session` or `session:window`); default = current window.
- `--config`  optional TOML (see `config.example.toml`); omit for plain defaults.
- `--bind`    HTTP listen address (default `127.0.0.1:8080`).
- `--interval` browser re-capture cadence in ms (default `500`).

Then open the printed URL. Errors (no tmux server / bad target) show as an in-page
banner rather than a failed request.

## Fonts

Each `[fonts."Name"]` entry is either embedded (`path = "..."` → base64 `@font-face`,
self-contained page) or referenced by an installed family (`system = "..."`). Font
family is an axis of the span style, so an override breaks a run exactly like a color
change — see `config.example.toml`.

Symbol glyphs (Nerd Font / powerline) are scaled to the cell via SVG: separators
(`U+E0B0–E0D4`) stretch to fill so segments tile seamlessly, other icons fit
proportionally. `config.kitty.toml` reproduces kitty's zero-config powerline
rendering by embedding kitty's bundled `Symbols Nerd Font Mono` and mapping the
Nerd-Font codepoint ranges to it.

## Status

Snapshot mode, single active window, full pane layout. Not yet: control-mode live
updates, scrollback, multi-window/session tab bar.
