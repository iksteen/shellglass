# tmuxsnitch

Mirror a **tmux** window's full pane layout as live **HTML** in your browser, with
Kitty-style `symbol_map` font overrides (map Unicode codepoint ranges to specific
fonts, e.g. Nerd Font glyph ranges).

Rendering is always **live**: a persistent `tmux -C` control-mode client is attached
once, each pane's terminal state is kept in a long-lived vt100 parser and updated from
tmux's incremental `%output` stream, then pushed to the browser over SSE. No polling,
no per-tick subprocess.

You can run it two ways:

- **Standalone** — mirror local tmux to a local browser (one process).
- **Hub + client** — a client streams its rendered frames to a remote **hub**, which
  hosts the session at a URL. Good for sharing a session off-box.

## Build

```sh
cargo build --release
# binary at ./target/release/tmuxsnitch  (examples below use it)
```

## Quickstart: standalone (one shell)

```sh
tmux new-session -d -s demo                       # a session to mirror
./target/release/tmuxsnitch --target demo --bind 127.0.0.1:8080
# open http://127.0.0.1:8080/
```

## Quickstart: hub + client (two shells)

The **secret** is the write capability; its hash `hex(sha256(secret))` is the
**session id**, a one-way read capability that goes in the view URL. The hub only
accepts pushes for session ids you pre-register with `--allow`.

### Shell A — the hub

```sh
# 1. pick a secret and compute its session id
SECRET='change-me-to-a-long-random-secret'
ID=$(./target/release/tmuxsnitch --key "$SECRET" --print-id)
echo "view at: http://127.0.0.1:8080/s/$ID"

# 2. run the hub, allowing that id (repeat --allow per client)
./target/release/tmuxsnitch --serve --bind 127.0.0.1:8080 --allow "$ID"
```

The hub needs no tmux and no config — it just relays what clients push.
For access from other machines, bind `0.0.0.0:8080` and use the host's address.

### Shell B — the client

```sh
tmux new-session -d -s demo                       # skip if you have a session

# same secret as the hub was configured for
export TMUXSNITCH_KEY='change-me-to-a-long-random-secret'
./target/release/tmuxsnitch --push http://127.0.0.1:8080 --target demo
```

The client prints its view URL on startup:

```
tmuxsnitch: pushing live to http://127.0.0.1:8080; view at http://127.0.0.1:8080/s/<id>
```

Open that `/s/<id>` URL in a browser. Viewing needs only the id (in the URL); pushing
needs the secret. A client whose key isn't on the hub's `--allow` list is rejected with
`403` at startup.

> Generate a real secret instead of the placeholder, e.g.
> `SECRET=$(head -c32 /dev/urandom | base64)`, and set the same value in both shells.

## Flags

| Flag | Applies to | Meaning |
|------|------------|---------|
| `--target <t>` | standalone, client | tmux target (`session` or `session:window`); default = current window |
| `--config <path>` | standalone, client | TOML config (fonts + `symbol_map`); omit for defaults |
| `--bind <addr>` | standalone, hub | HTTP listen address (default `127.0.0.1:8080`) |
| `--serve` | hub | run as a hub (no tmux/config needed) |
| `--allow <id>` | hub | a session id permitted to push; repeat per client. Others get `403` |
| `--push <url>` | client | hub base URL to push to |
| `--key <secret>` | client, `--print-id` | secret key (or `TMUXSNITCH_KEY` env var) |
| `--print-id` | — | print the session id for `--key` and exit |

The session id is portable: `tmuxsnitch --key K --print-id` equals
`printf %s K | sha256sum`.

## How it works

```
tmux -C (control mode)  ── %output ─►  per-pane vt100 parsers (seeded once via capture-pane)
  → parser-agnostic StyledCell grid   → symbol_map font resolution
  → HTML (absolute-positioned panes, coalesced <span> runs)
  → SSE fragment on a watch channel  → browser swaps #screen
```

Live tracking follows the target session's **current window** and needs the tmux server
running at launch. Attaching a control-mode client sizes it to the current window so it
won't resize your real session. Errors (no tmux server / bad target) show as an in-page
banner rather than a failed request. In hub mode the client renders everything and
pushes frames over a **single persistent streaming connection** (not a request per
frame), so throughput isn't gated by round-trip latency; the hub just stores and
re-serves the latest CSS + fragment. If the connection drops (hub restart, network
blip) the client re-registers and reconnects automatically.

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

## Security notes

- The **secret** is a bearer capability. Anyone who has it can push to that session;
  anyone with the **view URL** can watch. Use a long random secret and share the URL
  only with people who should see the session.
- `--push` speaks plain HTTP with the secret in a header. Run it over a trusted network
  or behind a TLS-terminating reverse proxy.
- The hub trusts allowed clients: it caps request bodies at 64 MB (embedded fonts are
  large) but does not otherwise rate-limit. Don't expose an open hub to the internet.

## Status

Control-mode live rendering, standalone + client/hub push, single active window, full
pane layout. Not yet: scrollback, multi-window/session tab bar, window switching within
a session, TLS on the push transport.
