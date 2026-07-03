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

The input backend is swappable. Instead of tmux, `--exec` mirrors an **interactive
command in a PTY** — the `script(1)` model: the command runs in your terminal and the
browser watches. No tmux needed; one command, no panes.

```sh
tmuxsnitch --exec bash -l                 # standalone: work in this shell, watch at :8080
tmuxsnitch --push https://hub … --key … --exec bash   # or stream it to a hub
```

`--exec` takes the command plus its args, so put it **last**. The terminal is switched to
raw mode for the session and restored when the command exits (which also quits
tmuxsnitch). Unix only.

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

The **secret** is the write capability; its hash — `hex(argon2id(secret))`, a
one-way, memory-hard derivation — is the **session id**, a read capability that
goes in the view URL. The hub only accepts pushes for session ids you pre-register
with `--allow`.

### Shell A — the hub

```sh
# 1. pick a secret and compute its session id (needed for the hub's --allow)
SECRET='change-me-to-a-long-random-secret'
ID=$(./target/release/tmuxsnitch --key "$SECRET" --print-id)

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
| `--exec <cmd>…` | standalone, client | mirror an interactive PTY command instead of tmux (put last) |
| `--config <path>` | standalone, client | TOML config (fonts + `symbol_map`); omit for defaults |
| `--bind <addr>` | standalone, hub | HTTP listen address (default `127.0.0.1:8080`) |
| `--serve` | hub | run as a hub (no tmux/config needed) |
| `--allow <id>` | hub | a session id permitted to push; repeat per client. Others get `403` |
| `--push <url>` | client | hub base URL to push to |
| `--key <secret>` | client, `--print-id` | secret key (or `TMUXSNITCH_KEY` env var) |
| `--print-id` | — | print the session id for `--key` and exit |
| `--tls-cert <path>` / `--tls-key <path>` | hub | serve HTTPS with your own PEM cert chain + key |
| `--acme-domain <d>` | hub | auto-obtain a cert via ACME/Let's Encrypt (repeat per domain) |
| `--acme-email <e>` | hub | contact email for the ACME account |
| `--acme-cache <dir>` | hub | persist ACME account + certs across restarts (recommended) |
| `--acme-production` | hub | use Let's Encrypt production (default: staging) |

The session id is `hex(argon2id(secret))` with a fixed application salt — a
memory-hard derivation, so a weak secret can't be cheaply brute-forced from the
public id. It's computed once per client connection, not per frame. Use
`--print-id` to obtain the id for a secret (there's no one-line shell equivalent).

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
referenced by `default_font`/`symbol_map` is located on the host running tmuxsnitch —
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

## Security notes

- The **secret** is a bearer capability. Anyone who has it can push to that session;
  anyone with the **view URL** can watch. Use a long random secret and share the URL
  only with people who should see the session.
- The secret travels in a header on `/register` and `/stream`. Terminate TLS so it
  isn't sent in the clear. The hub can do this itself:

  ```sh
  # your own certificate
  tmuxsnitch --serve --bind 0.0.0.0:443 --allow "$ID" \
    --tls-cert /etc/ssl/hub.crt --tls-key /etc/ssl/hub.key

  # or automatic Let's Encrypt (needs a public DNS name resolving to this host,
  # and port 443 reachable — the TLS-ALPN-01 challenge is served on the same socket)
  tmuxsnitch --serve --bind 0.0.0.0:443 --allow "$ID" \
    --acme-domain hub.example.com --acme-email you@example.com \
    --acme-cache /var/lib/tmuxsnitch/acme --acme-production
  ```

  ACME defaults to Let's Encrypt **staging** (untrusted certs, generous rate limits)
  so you can test the plumbing; add `--acme-production` for real certs. Always set
  `--acme-cache` or the account + certificate are re-issued on every restart.
  Otherwise, run behind a TLS-terminating reverse proxy (Traefik, nginx, …). The
  client's `--push` URL just needs to be `https://…`, and the hub honors
  `X-Forwarded-Proto`/`X-Forwarded-Host` so the view URL it prints on connect
  matches the proxy's public address, not the hub's internal bind.
- The hub trusts allowed clients: it caps the `/register` body at 64 MB (uploaded fonts
  are large) but does not otherwise rate-limit. Don't expose an open hub to the internet.

## Status

Control-mode live rendering, standalone + client/hub push, single active window, full
pane layout, optional hub TLS (own cert or ACME/Let's Encrypt). Not yet: scrollback,
multi-window/session tab bar, window switching within a session.
