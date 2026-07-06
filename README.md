# shellglass

A live glass over your shell: mirror a terminal session as live **HTML** in your browser.
You run an interactive command in a pseudo-terminal (the `script(1)` model) — it runs in
your terminal, the browser watches. Rendering is always **live** — terminal state is kept
in a long-lived vt100 parser and pushed to the browser over SSE, with no polling and no
per-tick subprocess — and carries Kitty-style `symbol_map` font overrides (map Unicode
codepoint ranges to specific fonts, e.g. Nerd Font glyph ranges).

Two ways to serve it:

- **Standalone** — mirror to a local browser (one process).
- **Hub + client** — a client streams its rendered frames to a remote **hub** that hosts
  the session at a URL. Good for sharing off-box.

```sh
shellglass serve                                  # mirror your $SHELL, watch at :8080
shellglass serve -- bash -l                        # or a specific command
shellglass push https://hub --key … -- bash        # or stream it to a hub
```

Each mode is a subcommand: `serve` (self-contained local viewer), `push` (stream to a
hub), `hub` (run a hub), plus `gen-key` / `print-id` helpers. Run `shellglass <cmd>
--help` for a command's flags. The command to mirror goes **last**, after any flags (use
`--` to separate it); omit it to mirror your `$SHELL`. The terminal is switched to raw
mode for the session and restored when the command exits (which also quits shellglass).
Unix only.

## Build

```sh
cargo build --release
# binary at ./target/release/shellglass  (examples below use it)
```

## Quickstart: standalone (one shell)

```sh
./target/release/shellglass serve --bind 127.0.0.1:8080     # mirror your $SHELL
# open http://127.0.0.1:8080/ — the browser mirrors this terminal live
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

The hub needs no config — it just relays what clients push.
For access from other machines, bind `0.0.0.0:8080` and use the host's address.

### Shell B — the client

```sh
# same secret as the hub was configured for
export SHELLGLASS_KEY='change-me-to-a-long-random-secret'
./target/release/shellglass push http://127.0.0.1:8080 -- bash -l
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
| `hub` | run as a hub: relay clients' pushes (no config needed) |
| `gen-key` | generate a random secret key, print it with its session id, and exit |
| `print-id` | print the session id for `--key` and exit |

Flags by command:

| Flag | Commands | Meaning |
|------|----------|---------|
| `[CMD]…` (positional) | serve, push | interactive command to mirror in a PTY; put it last (after `--`). Omit for your `$SHELL` |
| `--config <path>` | serve, push | TOML config (fonts, `symbol_map`, `template`); omit for defaults |
| `--bind <addr>` | serve, hub | HTTP listen address (default `127.0.0.1:8080`) |
| `--ssh-bind <addr>` | serve, hub | also serve a read-only ANSI view over SSH here; connect with `ssh -p <port> …` |
| `--ssh-host-key <path>` | serve, hub | OpenSSH host key for the SSH view (generated + persisted 0600 if absent) |
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
command in a PTY  ── output bytes ─►  long-lived vt100 parser
  → parser-agnostic StyledCell grid   → rectangle diffs (changed cells only)
  → SSE deltas, encoded once per frame for all viewers
  → viewer.js renders cells → HTML in the browser
```

The command runs in a pseudo-terminal you drive from your own terminal (the `script(1)`
model): its output is teed to your screen immediately and, in parallel, fed to a
long-lived vt100 parser snapshotted at up to 30fps. Each viewer gets a full frame on
connect, then only the **changed rectangles** — a keystroke costs bytes, not a full
screen — and a small baked-in renderer (`/viewer.js`) turns cells into HTML
client-side. Terminal resizes (`SIGWINCH`) reflow both the PTY and the browser. In hub
mode the client pushes frames over a **single persistent streaming connection** (not a
request per frame), so throughput isn't gated by round-trip latency; the hub diffs them
per session and re-serves the client's CSS, fonts, and render config. If the connection
drops (hub restart, network blip) the client re-registers and reconnects automatically —
and the local session pauses cleanly, showing the outage in your terminal until it's
back.

## Read-only SSH view

Add `--ssh-bind <addr>` to `serve` or `hub` to also expose the session as a **read-only
ANSI terminal view** over SSH — a live mirror in a plain terminal, no browser needed:

```sh
# standalone: any username connects
./target/release/shellglass serve --ssh-bind 127.0.0.1:2222 -- htop
ssh -p 2222 x@127.0.0.1

# hub: the session id is the SSH username
./target/release/shellglass hub --bind 127.0.0.1:8080 --ssh-bind 0.0.0.0:2222 --allow "$ID"
ssh -p 2222 "$ID"@hub.example.com
```

The **session id is the SSH username** — it's already the public read capability (the
same id that goes in the `/s/<id>` URL), so there's nothing to enter. The view is strictly
read-only: input is dropped except `q` / Ctrl-C / Ctrl-D, which disconnect. A viewer
terminal smaller than the session shows a top-left crop with a one-line status notice, and
reflows on resize.

The SSH server uses its **own** ed25519 host key, kept separate from the machine's
`/etc/ssh` identity on purpose — a shared key would extend the host's SSH trust to this
accept-any read-only endpoint. Point `--ssh-host-key <path>` at a key to reuse across
runs; without it a key is generated and persisted (0600) under
`$XDG_STATE_HOME/shellglass/` so its fingerprint stays stable. The SSH view is optional
and unsupervised: if it can't bind (e.g. a privileged port), the HTTP mirror still runs.

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

**Lines, blocks and powerline arrows are drawn, not fonted.** Box-drawing and block
elements (`U+2500–259F`), the legacy-computing mosaics (sextants `U+1FB00–1FB3B` and
one-eighth bars `U+1FB70–1FB7B`) and the powerline arrow separators (`U+E0B0–E0B3`) are
synthesized as geometry on a `<canvas>` laid over the text — snapped to device pixels, so
lines stay crisp, tile without seams, and render mixed light/heavy junctions (`┿ ╂ ┝`)
faithfully at any zoom or display scale (including moving a window between a HiDPI and a
regular monitor). These need **no font at all**, and the real glyph is kept underneath as
transparent text so selection and copy/paste still work. Only the long tail — smooth-mosaic
wedges, seven-segment and the rounded/flame powerline separators (`U+E0B4–E0D4`), plus
anything you point `symbol_map` at — falls through to the SVG-scaled font path below.

`symbol_map` is still useful alongside the stack: its matched glyphs are SVG-scaled to
lock to exactly one cell (so they tile seamlessly), which plain fallback — rendering at
the font's own advance — doesn't do. The remaining separators (`U+E0B4–E0D4`) stretch to
fill; other icons fit proportionally. `config.kitty.toml` reproduces kitty's zero-config
powerline rendering by serving kitty's bundled `Symbols Nerd Font Mono` as a fallback.

## Templating

The built-in page is a dark chrome with a **CRT toggle** in the nav — a pure-CSS CRT
overlay (scanlines, phosphor bloom, flicker, vignette), off by default and remembered
per browser in localStorage.

For a fully custom page, point `template` at a full HTML document with three tokens
the renderer fills at serve time:

| Token | Filled with |
|-------|-------------|
| `{{style}}` | the generated `<style>` — terminal cell CSS + served-font `@font-face` |
| `{{screen}}` | the live `<div id="screen">…</div>` the SSE updater swaps (keep the id) |
| `{{script}}` | the `<script>` blocks that boot the live renderer (config + `/viewer.js` loader) |

```toml
# config.toml
template = "my-viewer.html"
```

Everything around those tokens is yours — nav, wrapper, footer, extra `<style>`. Only
`{{screen}}`'s `#screen` id is load-bearing (the updater targets it); the others can be
placed anywhere in the document. Terminal content that happens to contain a literal
token string is left intact. In hub mode the client pushes its template to the hub, so
custom pages work off-box too.

## Security notes

- The **secret** is a bearer capability. Anyone who has it can push to that session;
  anyone with the **view URL** can watch. Use a long random secret and share the URL
  only with people who should see the session.
- The secret travels in a header on the `/push` WebSocket upgrade. Terminate TLS so it
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
  matches the proxy's public address, not the hub's internal bind. The push is a
  WebSocket (`/push`), so forward the `Upgrade`/`Connection` headers — nginx then
  tunnels it without buffering (its default `proxy_request_buffering on` would
  otherwise stall the stream). Viewer streams (`/s/<id>/events`, SSE) send
  `X-Accel-Buffering: no`, so nginx won't buffer those either.
- The hub trusts allowed clients: it caps a single pushed WebSocket message at 16 MB
  (a full frame with fonts is smaller) but does not otherwise rate-limit the content.
  A bad-key flood is bounded (concurrent auth hashes are capped and each rejection is
  logged for fail2ban), but don't expose an open hub to the internet.
- The read-only SSH view (`--ssh-bind`) authorizes by the **session id in the username** —
  the same read capability as the view URL. It accepts any connection and shows the session
  read-only, so treat exposing it like sharing the view URL. It uses a dedicated ed25519
  host key, never the machine's `/etc/ssh` key.

## Status

Mirror an interactive command in a PTY (the `script(1)` model, one screen) with live
rendering, standalone + client/hub push, an optional read-only SSH view (`--ssh-bind`),
viewer templating (built-in page with a CRT toggle, or a custom template), and optional
hub TLS (own cert or ACME/Let's Encrypt). Not yet: scrollback, multiple sessions/panes in
one view.
