# shellglass

A live glass over your shell: mirror a terminal session as live **HTML** in your browser.
Rendering is always **live** — terminal state is kept in a long-lived vt100 parser and
pushed to the browser over SSE, with no polling and no per-tick subprocess — and carries
Kitty-style `symbol_map` font overrides (map Unicode codepoint ranges to specific fonts,
e.g. Nerd Font glyph ranges).

Two input backends:

- **tmux** — attach a persistent `tmux -C` control-mode client and mirror a window's full
  multi-pane layout, updated live from tmux's incremental `%output` stream.
- **PTY** (`--exec`) — run any interactive command in a pseudo-terminal (the `script(1)`
  model): the command runs in your terminal, the browser watches. No tmux; one pane.

Two ways to serve it:

- **Standalone** — mirror to a local browser (one process).
- **Hub + client** — a client streams its rendered frames to a remote **hub** that hosts
  the session at a URL. Good for sharing off-box.

```sh
shellglass serve --target work                         # mirror tmux session "work", watch at :8080
shellglass serve --exec bash -l                        # or mirror a PTY: work in this shell, watch it
shellglass push https://hub --key … --exec bash        # or stream it to a hub
```

Each mode is a subcommand: `serve` (self-contained local viewer), `push` (stream to a
hub), `hub` (run a hub), plus `gen-key` / `print-id` helpers. Run `shellglass <cmd>
--help` for a command's flags. For `--exec`, the command plus its args go **last**. The terminal is switched to raw mode
for the session and restored when the command exits (which also quits shellglass). Unix
only.

## Build

```sh
cargo build --release
# binary at ./target/release/shellglass  (examples below use it)
```

## Quickstart: standalone (one shell)

```sh
tmux new-session -d -s demo                       # a session to mirror
./target/release/shellglass serve --target demo --bind 127.0.0.1:8080
# open http://127.0.0.1:8080/
```

## Quickstart: hub + client (two shells)

The **secret** is the write capability; its hash — `hex(argon2id(secret))`, a
one-way, memory-hard derivation — is the **session id**, a read capability that
goes in the view URL. The hub only accepts pushes for session ids you pre-register
with `--allow`.

### Shell A — the hub

```sh
# 1. mint a secret and its session id (the id is needed for the hub's --allow)
./target/release/shellglass gen-key
#   key: <secret>   -> keep private; the client pushes with it
#   id:  <id>       -> public; add to the hub's --allow
SECRET='<the printed key>'
ID='<the printed id>'

# 2. run the hub, allowing that id (repeat --allow per client)
./target/release/shellglass hub --bind 127.0.0.1:8080 --allow "$ID"
```

The hub needs no tmux and no config — it just relays what clients push.
For access from other machines, bind `0.0.0.0:8080` and use the host's address.

### Shell B — the client

```sh
tmux new-session -d -s demo                       # skip if you have a session

# same secret as the hub was configured for
export SHELLGLASS_KEY='change-me-to-a-long-random-secret'
./target/release/shellglass push http://127.0.0.1:8080 --target demo
```

The client prints its view URL on startup:

```
shellglass: pushing live to http://127.0.0.1:8080; view at http://127.0.0.1:8080/s/<id>
```

Open that `/s/<id>` URL in a browser. Viewing needs only the id (in the URL); pushing
needs the secret. A client whose key isn't on the hub's `--allow` list is rejected with
`403` at startup.

> `gen-key` mints a secure 256-bit secret and prints its session id in one step;
> use the printed key as `SHELLGLASS_KEY` on the client and the printed id in the
> hub's `--allow`.

## Commands

`shellglass <command>` — run `shellglass <command> --help` for its full flags.

| Command | What it does |
|---------|--------------|
| `serve` | self-contained: render locally and serve the viewer over HTTP |
| `push <url>` | client: render locally, stream frames to the hub at `<url>` |
| `hub` | run as a hub: relay clients' pushes (no tmux/config needed) |
| `gen-key` | generate a random secret key, print it with its session id, and exit |
| `print-id` | print the session id for `--key` and exit |

Flags by command:

| Flag | Commands | Meaning |
|------|----------|---------|
| `--target <t>` | serve, push | tmux target (`session` or `session:window`); default = current window |
| `--exec <cmd>…` | serve, push | mirror an interactive PTY command instead of tmux (put last) |
| `--config <path>` | serve, push | TOML config (fonts, `symbol_map`, `template`); omit for defaults |
| `--bind <addr>` | serve, hub | HTTP listen address (default `127.0.0.1:8080`) |
| `--key <secret>` | push, print-id | secret key (or `SHELLGLASS_KEY` env var) |
| `--allow <id>` | hub | a session id permitted to push; repeat per client. Others get `403` |
| `--tls-cert <path>` / `--tls-key <path>` | hub | serve HTTPS with your own PEM cert chain + key |
| `--acme-domain <d>` | hub | auto-obtain a cert via ACME/Let's Encrypt (repeat per domain) |
| `--acme-email <e>` | hub | contact email for the ACME account |
| `--acme-cache <dir>` | hub | persist ACME account + certs across restarts (recommended) |
| `--acme-production` | hub | use Let's Encrypt production (default: staging) |

The session id is `hex(argon2id(secret))` with a fixed application salt — a
memory-hard derivation, so a weak secret can't be cheaply brute-forced from the
public id. It's computed once per client connection, not per frame. Use
`print-id` to obtain the id for a secret (there's no one-line shell equivalent).

## How it works

```
tmux -C (control mode)  ── %output ─►  per-pane vt100 parsers (seeded once via capture-pane)
  → parser-agnostic StyledCell grid   → symbol_map font resolution
  → HTML (absolute-positioned panes, coalesced <span> runs)
  → SSE fragment on a watch channel  → browser swaps #screen
```

The `--exec` PTY backend joins this pipeline at the vt100-parser stage — one parser, one
pane — so everything downstream (grid → HTML → SSE) is shared with the tmux path.

Live tracking follows the target session's **current window** and needs the tmux server
running at launch. Attaching a control-mode client sizes it to the current window so it
won't resize your real session. Errors (no tmux server / bad target) show as an in-page
banner rather than a failed request. In hub mode the client renders everything and
pushes frames over a **single persistent streaming connection** (not a request per
frame), so throughput isn't gated by round-trip latency; the hub just stores and
re-serves the latest CSS + fragment, plus the fonts the client uploaded. If the
connection drops (hub restart, network blip) the client re-registers and reconnects
automatically.

## Fonts

`default_font` can be a single family or a **fallback stack** — `["Text Font", "Nerd
Font"]`. The browser resolves each character against the stack in order, so a Nerd Font
listed after your text font covers every glyph the text font lacks. That reproduces
Kitty's fallback with no `symbol_map` ranges at all (see `config.kitty.toml`).

By default `default_font` is `["monospace", "Symbols Nerd Font Mono"]`, so Nerd-Font
symbol glyphs render out of the box with **no config** if `Symbols Nerd Font Mono` is
installed system-wide (it's part of the standard Nerd Fonts packages). Override
`default_font` to use a different text font or symbol font.

**Fonts are served, so viewers render them without a local install.** Every family
referenced by `default_font`/`symbol_map` is located on the host running shellglass —
via [`fontdb`] (a pure-Rust, cross-platform system font database; Linux, macOS and
Windows, no subprocess), or an explicit `[fonts."Name"].path` — its file read once at
startup, and served: standalone at `/fonts/<i>`, the hub **per
session** at `/s/<id>/fonts/<i>` (so two clients' fonts can't clash). The page's
`@font-face` points at those URLs; responses are Brotli/gzip-compressed and sent with a
day-long `Cache-Control` so a browser fetches each font once. A single face is extracted
from a `.ttc` collection (e.g. macOS's `Menlo.ttc`) and served as a standalone web font.
A font that can't be located is a soft failure (warn + skip); the browser just falls
back. A `[fonts."Name"]` entry is optional — use `path` to serve a specific file
(`.ttf`/`.otf`/`.ttc`/`.woff`/`.woff2`), or `system = "Other Name"` when the installed
family name differs from the key.

[`fontdb`]: https://docs.rs/fontdb

`symbol_map` is still useful alongside the stack: its matched glyphs are SVG-scaled to
lock to exactly one cell (powerline separators tile seamlessly), which plain fallback —
rendering at the font's own advance — doesn't do. Separators (`U+E0B0–E0D4`) stretch to
fill; other icons fit proportionally. `config.kitty.toml` reproduces kitty's zero-config
powerline rendering by serving kitty's bundled `Symbols Nerd Font Mono` as a fallback.

## Templating

Two built-in themes ship: `default` (dark) and `crt` (the same, with a pure-CSS CRT
overlay — scanlines, phosphor bloom, flicker, vignette). Pick one in your `--config`:

```toml
theme = "crt"
```

For a fully custom page, point `template` at a full HTML document (this overrides
`theme`) with three tokens the renderer fills at serve time:

| Token | Filled with |
|-------|-------------|
| `{{style}}` | the generated `<style>` — terminal cell CSS + served-font `@font-face` |
| `{{screen}}` | the live `<div id="screen">…</div>` the SSE updater swaps (keep the id) |
| `{{script}}` | the `<script>` that subscribes to the event stream |

```toml
# config.toml
template = "my-viewer.html"
```

Everything around those tokens is yours — nav, wrapper, footer, extra `<style>`. Only
`{{screen}}`'s `#screen` id is load-bearing (the updater targets it); the others can be
placed anywhere in the document. Terminal content that happens to contain a literal
token string is left intact. In hub mode the client pushes its template to the hub, so
custom themes work off-box too.

## Security notes

- The **secret** is a bearer capability. Anyone who has it can push to that session;
  anyone with the **view URL** can watch. Use a long random secret and share the URL
  only with people who should see the session.
- The secret travels in a header on `/register` and `/stream`. Terminate TLS so it
  isn't sent in the clear. The hub can do this itself:

  ```sh
  # your own certificate
  shellglass hub --bind 0.0.0.0:443 --allow "$ID" \
    --tls-cert /etc/ssl/hub.crt --tls-key /etc/ssl/hub.key

  # or automatic Let's Encrypt (needs a public DNS name resolving to this host,
  # and port 443 reachable — the TLS-ALPN-01 challenge is served on the same socket)
  shellglass hub --bind 0.0.0.0:443 --allow "$ID" \
    --acme-domain hub.example.com --acme-email you@example.com \
    --acme-cache /var/lib/shellglass/acme --acme-production
  ```

  ACME defaults to Let's Encrypt **staging** (untrusted certs, generous rate limits)
  so you can test the plumbing; add `--acme-production` for real certs. Always set
  `--acme-cache` or the account + certificate are re-issued on every restart.
  Otherwise, run behind a TLS-terminating reverse proxy (Traefik, nginx, …). The
  client's `push` URL just needs to be `https://…`, and the hub honors
  `X-Forwarded-Proto`/`X-Forwarded-Host` so the view URL it prints on connect
  matches the proxy's public address, not the hub's internal bind.
- The hub trusts allowed clients: it caps the `/register` body at 64 MB (uploaded fonts
  are large) but does not otherwise rate-limit. Don't expose an open hub to the internet.

## Status

Two backends — tmux control-mode (full pane layout, single active window) and a PTY
(`--exec`, one pane) — with live rendering, standalone + client/hub push, viewer
templating (default + CRT themes), and optional hub TLS (own cert or ACME/Let's Encrypt).
Not yet: scrollback, multi-window/session tab bar, window switching within a session.
