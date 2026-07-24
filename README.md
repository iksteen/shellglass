# shellglass

A live glass over your shell: mirror a terminal session as live **HTML** in your
browser. You run an interactive command in a pseudo-terminal (the `script(1)` model) —
it runs in your terminal, the browser watches. Terminal state lives in a long-lived
vt100 parser and is pushed to the browser over SSE: no polling, no per-tick
subprocess. Inline images (kitty graphics, iTerm2, sixel) and Kitty-style
`symbol_map` font overrides are mirrored too.

Two ways to serve it:

- **Standalone** — mirror to a local browser (one process).
- **Hub + client** — a client streams its frames to a remote **hub** that hosts the
  session at a URL. Good for sharing off-box.

```sh
shellglass serve                                  # mirror your $SHELL, watch at :8080
shellglass serve -- bash -l                       # or a specific command
shellglass push https://hub --key … -- bash       # or stream it to a hub
```

The command to mirror goes **last**, after any flags (use `--` to separate it); omit it
to mirror your `$SHELL` on Unix or `%COMSPEC%` on Windows. The terminal is switched
to raw mode for the session and restored when the command exits (which also quits
shellglass). On Windows, shellglass uses ConPTY and requires Windows 10 version 1809
or newer; Windows Terminal is the recommended host.

## Build & quickstart

```sh
cargo build --release
./target/release/shellglass serve --bind 127.0.0.1:8080
# open http://127.0.0.1:8080/ — the browser mirrors this terminal live
```

From PowerShell on Windows:

```powershell
cargo build --release
.\target\release\shellglass.exe serve --bind 127.0.0.1:8080
# Omit the command for cmd.exe, or append: -- pwsh.exe -NoLogo
```

## Hub + client

The **secret key** is the write capability. Its hash — `hex(argon2id(secret))`, a
one-way, memory-hard derivation, so a weak secret can't be cheaply brute-forced —
is the **session id**: the read capability that goes in the view URL. The hub
accepts pushes only for pre-registered ids; it never sees secrets.

Shell A — the hub:

```sh
./target/release/shellglass gen-key
#   key: <secret>   -> keep private; the client pushes with it
#   id:  <id>       -> public; register it on the hub
./target/release/shellglass hub --bind 127.0.0.1:8080 --allow '<the id>'
```

Shell B — the client:

```sh
export SHELLGLASS_KEY='<the key>'
./target/release/shellglass push http://127.0.0.1:8080 -- bash -l
```

The **hub** announces the view URL when the session connects:

```
shellglass: session connected — view at http://127.0.0.1:8080/s/<id>/
```

(The hub is the authority on view URLs — only it knows the slug and its own
public address; the client just pushes.) Viewing needs only the URL; pushing
needs the secret — a key whose id isn't registered is rejected with `403` at
startup. For access from other machines, bind `0.0.0.0:8080` and use the
host's address.

**Friendlier view URLs.** `--allow <id>:<slug>` (e.g. `--allow "$ID:demo"`) serves
the session at `/s/demo` instead of `/s/<64-hex-id>` — the slug becomes the *only*
view route. Pushing is unchanged. Ids and slugs must be unique across `--allow`; a
collision or a non-URL-safe slug is a startup error.

## Commands

`shellglass <command>` — run `shellglass <command> --help` for its full flags.

| Command | What it does |
|---------|--------------|
| `serve` | self-contained: render locally and serve the viewer over HTTP |
| `push <url>` | client: render locally, stream frames to the hub at `<url>` |
| `hub` | run as a hub: relay clients' pushes (no config needed) |
| `sessions <url>` | manage a hub's sessions over its management API: list, add, remove — plus any session's recordings |
| `recordings <url>` | manage **your own** session recordings on a hub: list, get, delete (authorized by the session key) |
| `gen-key` | mint a random secret and print it with its session id (`--api` for an API credential) |
| `print-id` | print the session id for `--key` (`--api` for its API id) |

Flags by command:

| Flag | Commands | Meaning |
|------|----------|---------|
| `[CMD]…` (positional) | serve, push | interactive command to mirror in a PTY; put it last (after `--`). Omit for your `$SHELL` |
| `--config <path>` | serve, push | TOML config (fonts, `symbol_map`, `template`); omit for defaults. Falls back to `$SHELLGLASS_CONFIG` |
| `--bind <addr>` | serve, hub | HTTP listen address (default `127.0.0.1:8080`) |
| `--ssh-bind <addr>` | serve, hub | also serve a read-only ANSI view over SSH here |
| `--ssh-host-key <path>` | serve, hub | OpenSSH host key for the SSH view (generated + persisted 0600 if absent) |
| `--ssh-motd-file <path>` | serve, hub | file shown as a banner to each SSH viewer before the live view — raw bytes, all control characters preserved |
| `--ssh-motd-delay <n>` | serve, hub | seconds to show the MOTD banner (default 5) |
| `--key <secret>` | push, recordings, print-id | secret key (or `SHELLGLASS_KEY` env var) |
| `--key <api-key>` | sessions | management-API key (or `SHELLGLASS_API_KEY` env var) |
| `--allow <id>[:<slug>]` | hub | a session id permitted to push, optionally aliased to a view-URL slug; repeat per client. Others get `403` |
| `--api-allow <api-id>` | hub | an API id permitted to call the session-management API; repeat per caller. Without it, `/api` is off (404) |
| `--sessions-file <path>` | hub | persist the session registry across restarts (see [Managing hub sessions](#managing-hub-sessions-over-http)) |
| `--record-dir <dir>` | serve, hub | record sessions as timestamped native streams (see [Session recording](#session-recording)) |
| `--no-record` | push | decline recording on a hub that records (or `SHELLGLASS_NO_RECORD=true`) |
| `--api` | gen-key, print-id | mint/print in the API salt domain (for `--api-allow`) instead of the session domain |
| `--id-salt <ext>` | hub, gen-key, print-id | optional per-system salt extension (or `SHELLGLASS_ID_SALT`); one value per hub, used by every command deriving ids for it — see [security notes](#security-notes) |
| `--tls-cert <path>` / `--tls-key <path>` | hub | serve HTTPS with your own PEM cert chain + key |
| `--acme-domain <d>` | hub | auto-obtain a cert via ACME/Let's Encrypt (repeat per domain) |
| `--acme-email <e>` | hub | contact email for the ACME account |
| `--acme-cache <dir>` | hub | persist ACME account + certs across restarts (recommended) |
| `--acme-production` | hub | use Let's Encrypt production (default: staging) |

## Managing hub sessions over HTTP

An external tool can add and remove sessions at runtime instead of restarting the
hub with new `--allow` flags. The API is **off by default**: it only exists when
the hub is started with at least one `--api-allow <api-id>`.

API callers authenticate like sessions do — a secret key, screened by its argon2id
hash — but in a **separate salt domain**, so a session key is never an API
credential and vice versa. Mint one with `gen-key --api` and put the printed API
id on `--api-allow`.

```sh
# operator: mint an API credential and start the hub with it
shellglass gen-key --api          # key: <API_KEY>   api-id: <API_ID>
shellglass hub --bind 0.0.0.0:8080 --api-allow <API_ID>

# the built-in client (key via --key or SHELLGLASS_API_KEY):
export SHELLGLASS_API_KEY=<API_KEY>
shellglass sessions https://hub add <session-id> --slug demo
shellglass sessions https://hub list
shellglass sessions https://hub remove --slug demo   # or: remove --id <session-id>
shellglass sessions https://hub recordings list --slug demo         # or --id
shellglass sessions https://hub recordings get --slug demo <name>   # -o -  for stdout
shellglass sessions https://hub recordings delete --slug demo <name>

# or any HTTP client (Authorization: Bearer <API_KEY>):
curl -X POST -H "Authorization: Bearer $SHELLGLASS_API_KEY" \
     -d '{"id":"<session-id>","slug":"demo"}' https://hub/api/sessions
curl -H "Authorization: Bearer $SHELLGLASS_API_KEY" https://hub/api/sessions
curl -X DELETE -H "Authorization: Bearer $SHELLGLASS_API_KEY" https://hub/api/sessions/by-slug/demo
```

| Route | Effect |
|-------|--------|
| `POST /api/sessions` | register `{"id": <session-id>, "slug"?: <slug>}` — the public id from `print-id`, never a key. `201`; `409` id or slug taken; `400` malformed |
| `DELETE /api/sessions/by-id/<id>` | remove by **session id**. `204`; `404` unknown |
| `DELETE /api/sessions/by-slug/<slug>` | remove by **view slug**. `204`; `404` unknown |
| `GET /api/sessions` | list `[{id, slug, live, webViewers, sshViewers}]` — `live` means an operator is currently pushing; `webViewers`/`sshViewers` are the current live viewer counts per transport |
| `GET /api/sessions/by-id/<id>/recordings` | list a session's recordings `[{name, bytes}]`, oldest first (see [Session recording](#session-recording)). Works after the session is deleted — the files are the artifact |
| `GET /api/sessions/by-slug/<slug>/recordings` | same, resolved through the registry (`404` for an unknown slug) |
| `GET /api/sessions/by-{id,slug}/…/recordings/<name>` | fetch one recording (`application/x-ndjson`) |
| `DELETE /api/sessions/by-{id,slug}/…/recordings/<name>` | delete one recording. `204`; `404` unknown |

Removal by id and by slug are separate routes on purpose: an un-aliased session's
slug **is** its id, so a single guessing route could delete the wrong thing.
Deleting a session kicks its live pusher (whose next reconnect gets `403`) and
drops everything the hub stored for it. A newly added session is viewable
immediately: until its pusher connects, `/s/<slug>` serves the built-in page in
the operator-offline state and switches to the live session on the first push.
`--allow` entries are ordinary registry entries — same placeholder, deletable
over the API like any other.

Each session also exposes `GET /s/<slug>/snapshot` — a one-shot JSON blob of the
current screen state (the same full-frame message `/events` sends first, no
version hello, no stream), for a consumer that wants a point-in-time read without
holding an SSE connection open. It needs no auth — the slug is the read
capability, same as the view page. `serve` exposes it at `/snapshot` too.

**Lifetime.** By default the registry lives in memory: a restart forgets API
changes and re-seeds from `--allow`. Pass `--sessions-file <path>` to make it
durable — every mutation atomically rewrites the file (public ids and slugs only,
no secrets). On startup, a loadable file **is** the registry and `--allow` is
ignored (announced on stderr); a missing file is seeded from `--allow`; a corrupt
file is a startup error, never a silent fall back to `--allow` — re-seeding could
resurrect sessions the API deleted.

## Session recording

`--record-dir <dir>` records sessions as **timestamped native shellglass
streams** — `.sgs` files, one JSON line per event:

- a header (`{"shellglass":1,"protocol":…,"start":<unix-ms>}`), then
- one `[ms_since_start, <message>]` line per push-protocol message, verbatim:
  the register (page CSS, render config, the base64 font bundle), `{"blob":…}`
  image payloads, and the wire messages themselves.

A recording is the session's push transcript, so it is **self-contained**
(fonts, template, and images included, unlike a plain terminal capture) and
replayable at full mirror fidelity by anything that speaks the wire format.
Consumers dispatch lines the way the hub does: the first message is the
register, a `blob` key is an image payload, everything else is a wire message.

On a **hub**, every pushed session records while its pusher is connected: one
file per push connection at `<dir>/<session-id>/<start-millis>.sgs`, announced
in the hub log when the connection ends, and enumerable/retrievable through the
management API (see the route table above). A pusher declines with
`push --no-record` (or `SHELLGLASS_NO_RECORD=true`) — the flag rides the
register message. On **serve**, the same format records the standalone session
(one file per run, in `<dir>` directly) with a synthesized register, so the
file looks exactly like a hub recording.

**Session owners manage their own recordings** with the session key — the
same secret `push` uses. It both authorizes and names the session, so only
that session's files are ever reachable, and neither an id nor a slug rides
the request:

```sh
export SHELLGLASS_KEY=<secret>            # the push key
shellglass recordings https://hub list
shellglass recordings https://hub get 1752574530123.sgs        # saves ./<name>
shellglass recordings https://hub get 1752574530123.sgs -o -   # to stdout
shellglass recordings https://hub delete 1752574530123.sgs

# or any HTTP client (x-shellglass-key: <secret>, like /push):
curl -H "x-shellglass-key: $SHELLGLASS_KEY" https://hub/recordings
curl -H "x-shellglass-key: $SHELLGLASS_KEY" https://hub/recordings/<name>
curl -X DELETE -H "x-shellglass-key: $SHELLGLASS_KEY" https://hub/recordings/<name>
```

Management and owner have the same three verbs — list, retrieve, delete — the
only difference being scope: the management API reaches any session's
recordings, the session key only its own. Otherwise, recordings are plain
files: rotation, retention, and cleanup are yours (a deleted session keeps
its files, still listable by id over the management API).

## How it works

```
command in a PTY  ── output bytes ─►  long-lived vt100 parser
  → parser-agnostic StyledCell grid   → per-line diffs (changed spans only)
  → SSE deltas, encoded once per frame for all viewers
  → viewer.js paints cells on a canvas in the browser
```

The command runs in a pseudo-terminal you drive from your own terminal (the
`script(1)` model): its output is teed to your screen immediately and, in
parallel, fed to a long-lived vt100 parser snapshotted at up to 30fps. Each
viewer gets a full frame on connect, then only the **changed spans** — a
keystroke costs bytes, not a full screen — and a small baked-in renderer
(`/viewer.js`) paints cells client-side (see [Templating](#templating) for the
rendering model). Terminal resizes (`SIGWINCH`)
reflow both the PTY and the browser. In hub mode the client pushes frames over a
**single persistent WebSocket** (not a request per frame), so throughput isn't
gated by round-trip latency; the hub relays them per session and re-serves the
client's CSS, fonts, and render config. If the connection drops (hub restart,
network blip) the client re-registers and reconnects automatically, showing the
outage in your terminal until it's back.

## Inline images

When the session shows an image via any of the three inline-image protocols,
viewers see it too — positioned at its terminal cell, scrolling with the content:

| Protocol | Emitted by (e.g.) | Coverage |
|----------|-------------------|----------|
| **kitty graphics** (`ESC _G`) | `kitten icat`, `timg`, `chafa -f kitty` | PNG and raw RGB/RGBA (re-encoded to PNG), chunked transfers, zlib compression, and the direct / file / temp-file / shared-memory transmission mediums |
| **iTerm2** (`OSC 1337 File`) | `imgcat`, `wezterm imgcat`, `chafa -f iterm` | single-shot and multipart transfers; browser-native image formats (PNG, JPEG, GIF, WebP) |
| **sixel** (`ESC P … q`) | `img2sixel`, `chafa -f sixel` | decoded and re-encoded to PNG (browsers don't render sixel) |

**The mirror only shows what your terminal shows.** At startup shellglass asks
the terminal which protocols it actually renders — kitty graphics via its query
escape, sixel via the Primary DA feature list, iTerm2 (which has no query) by a
small `TERM_PROGRAM` allowlist — and intercepts only those. A protocol your
terminal doesn't render passes through untouched, so no image appears on the web
that didn't appear in the terminal, and none is lost that did.

Images behave like they do in a cell-based terminal: they scroll with the text,
clip against the top edge on the way out, and disappear when the screen region
they occupy is cleared or overwritten. Hub mode included — late-joining viewers
get any image currently on screen. Frames carry only image *placements* (a
content hash); viewers fetch the bytes once over HTTP and cache them forever
(the URL is immutable), so a large on-screen image costs each viewer one
download — not a re-transmission inside every full frame, multiplied by every
viewer. Copying a selection still embeds the actual bitmap. The read-only SSH
view is cells-only and does not show images.

## Read-only SSH view

Add `--ssh-bind <addr>` to `serve` or `hub` to also expose the session as a
**read-only ANSI terminal view** over SSH — a live mirror in a plain terminal, no
browser needed:

```sh
# standalone: any username connects
./target/release/shellglass serve --ssh-bind 127.0.0.1:2222 -- htop
ssh -p 2222 x@127.0.0.1

# hub: the view handle (session id, or its slug if aliased) is the SSH username
./target/release/shellglass hub --bind 127.0.0.1:8080 --ssh-bind 0.0.0.0:2222 --allow "$ID"
ssh -p 2222 "$ID"@hub.example.com
```

The **view handle is the SSH username** — the same one that goes in the `/s/<…>`
URL — so there's nothing to enter. Input is dropped except `q` / Ctrl-C / Ctrl-D,
which disconnect. A viewer terminal smaller than the session shows a top-left
crop with a one-line status notice; a larger one centers the session in the extra
space (per axis); either way it reflows on resize.

Set `--ssh-motd-file <path>` to greet each viewer with a banner before the live
view starts (`--ssh-motd-delay <n>` seconds, default 5). The file is sent
**verbatim** — every control character is preserved, so ANSI colors, cursor
positioning, and terminal art all work; the operator authors it, so it's trusted.
It shows on the normal screen and the live view takes over the alternate screen,
so disconnecting restores it like a login MOTD.

The SSH server uses its **own** ed25519 host key, kept separate from the
machine's `/etc/ssh` identity on purpose — a shared key would extend the host's
SSH trust to this accept-any read-only endpoint. Point `--ssh-host-key <path>` at
a key to reuse across runs; without it one is generated and persisted (0600)
under `$XDG_STATE_HOME/shellglass/` so its fingerprint stays stable. The SSH view
is optional and unsupervised: if it can't bind (e.g. a privileged port), the HTTP
mirror still runs.

## Fonts

`default_font` can be a single family or a **fallback stack** — `["Text Font",
"Nerd Font"]`. The browser resolves each character against the stack in order, so
a Nerd Font listed after your text font covers every glyph the text font lacks —
Kitty's fallback behavior with no `symbol_map` ranges at all (see
`config.kitty.toml`). The default is `["monospace", "Symbols Nerd Font Mono"]`,
so Nerd-Font symbols render with **no config** if that font is installed
system-wide.

**Fonts are served, so viewers render them without a local install.** Every
family referenced by `default_font`/`symbol_map` is located on the host — via
[`fontdb`] (a pure-Rust system font database) or an explicit `[fonts."Name"].path`
— read once at startup, and served **by content hash**: `fonts/<sha256>`,
relative to the page. The hub stores each distinct font once however many
sessions push it (it derives the hash from the pushed bytes itself, so no
client can overwrite another's fonts; a font is evicted when its last
referencing session goes), and each session serves only its own fonts.
Content-addressed URLs never change meaning, so responses are compressed and
cached indefinitely. A single face is extracted from a `.ttc` collection; a
font that can't be located is a soft failure (warn + skip, the browser falls
back). In a `[fonts."Name"]` entry, `path` serves a specific file
(`.ttf`/`.otf`/`.ttc`/`.woff`/`.woff2`) and `system = "Other Name"` maps to a
differently-named installed family. `weight_boost = false` turns off the
renderer's kitty-parity double-draw (which thickens anti-aliased midtones) for
that family — for fonts whose own hinting already looks right and where the
boost reads as too heavy; omit it to keep the boost, as every other font does.

[`fontdb`]: https://docs.rs/fontdb

**Lines, blocks and powerline arrows are drawn, not fonted.** Box-drawing and
block elements (`U+2500–259F`), the legacy-computing mosaics (sextants
`U+1FB00–1FB3B`, one-eighth bars `U+1FB70–1FB7B`) and the powerline arrows
(`U+E0B0–E0B3`) are synthesized as geometry on a `<canvas>` over the text —
snapped to device pixels, so lines stay crisp, tile without seams, and junctions
(`┿ ╂ ┝`) render faithfully at any zoom or display scale. The real glyph is kept
underneath as transparent text so selection and copy/paste still work.

`symbol_map` is still useful alongside the stack: its matched glyphs are
SVG-scaled to lock to exactly one cell (so they tile seamlessly), which plain
fallback — rendering at the font's own advance — doesn't do. The long tail
(rounded/flame powerline separators `U+E0B4–E0D4`, seven-segment, smooth
mosaics) takes this path: separators stretch to fill, other icons fit
proportionally.

## Templating

The picture paints on a canvas, by the terminal's own rules (kitty is the
reference): ink seated inside the cell box, decorations clamped into the cell
and drawn through spaces, DECSCUSR cursor shapes, images under later text. The
DOM underneath carries the same content as transparent ghost text, so native
select/copy/find work — what you see, what you highlight, and what Ctrl-C
copies are the same thing (the whole picture holds still while you select).

The built-in page is a dark chrome. `?cursor=smooth` makes the cursor glide
between cells over ~80ms instead of teleporting.

For a fully custom page, point `template` at a full HTML document with three
tokens the renderer fills at serve time:

| Token | Filled with |
|-------|-------------|
| `{{style}}` | the generated `<style>` — terminal cell CSS + served-font `@font-face` |
| `{{screen}}` | the live `<div id="screen">…</div>` the SSE updater swaps (keep the id) |
| `{{script}}` | the `<script>` blocks that boot the live renderer (config + `/viewer.js` loader) |

```toml
# config.toml
template = "my-viewer.html"
```

Everything around those tokens is yours — nav, wrapper, footer, extra `<style>`.
Only `{{screen}}`'s `#screen` id is load-bearing (the updater targets it). In hub
mode the client pushes its template to the hub, so custom pages work off-box too.

If your page fits the terminal to a box with `transform: scale`, dispatch a
`sg-zoom` event after changing the factor (so the canvas re-rasterizes crisp)
and give the page a non-transformed `<div id="sg-canvas-host">` around
`{{screen}}`, sized to the scaled rectangle: the renderer mounts its canvas
there, outside the transform, which keeps glyphs sharp in Safari (WebKit
resamples a transformed canvas layer). Both built-in pages do exactly this.

## Embedding in another page

One line, where the terminal should appear:

```html
<script src="https://hub.example.com/embed.js"
        data-src="https://hub.example.com/s/demo"></script>
```

By default the terminal renders **iframe-less** — straight into your page, no
frame to fight a strict CSP or to style around. The script replaces itself with
a `<shellglass-view>` element (also usable directly in markup):

```html
<shellglass-view src="https://hub.example.com/s/demo"></shellglass-view>
```

Three render modes, via a `mode` attribute (or `data-mode` on the script tag):

- **light** (default) — renders in your page's own DOM. Native selection and
  copy, nothing sandboxed. The viewer scopes its own CSS so it won't restyle
  your page.
- **shadow** — `mode="shadow"` renders in a shadow root, fully style-isolated
  from the host. (Selection across a shadow boundary is weaker on some browsers.)
- **iframe** — `mode="iframe"` is the classic sandboxed frame onto the `?embed`
  page. The raw form still works with no script at all:
  ```html
  <iframe src="https://hub.example.com/s/demo?embed"
          style="border:0;width:100%;height:24em"></iframe>
  ```

`data-src`, the element name, its `src`, and the `?embed` URL shape are the
**stable** contract — reconnects, upgrades and the operator-offline state all
happen inside. An embed always uses the built-in look; a custom `template`
doesn't apply (the session's fonts and colors do). Embedding a session is
exactly as public as its view URL — see the security notes below.

**Sizing.** An unstyled `<shellglass-view>` (light or shadow) renders at the
terminal's natural size. Give it a `width` and/or `height` in CSS and the
terminal scales to fill that box, letterboxed (aspect preserved), staying crisp
as it scales — so it behaves like a normal sized element. The `iframe` mode
fills its frame (default `100%` × `24em`) the same way.

**Window title.** The light/shadow modes don't touch your page's tab title.
Instead the session's title (OSC 0/2) rides a `shellglass-title` event on the
element, so you decide what to do with it:

```js
el.addEventListener("shellglass-title", (e) => {
  console.log("terminal title:", e.detail); // "" when the session clears it
});
```

**Same origin is the easy path.** Put the hub behind your reverse proxy on your
own domain (e.g. `example.com/term/ → hub`) and point `src` at that path —
every asset the viewer fetches is relative to `src`, so nothing is cross-origin
and **no CORS is needed**. For a genuinely cross-origin embed (the page and the
hub on different origins, iframe-less), run the hub — or standalone `serve` —
with `--cors-origin https://your-page.example` (repeat for several, or `*` for
any); the iframe mode needs none. Multiple iframe-less embeds on one page aren't
supported (they share one renderer instance) — use `mode="iframe"` for those.

## Security notes

- The **secret** is a bearer capability. Anyone who has it can push to that
  session; anyone with the **view URL** can watch. Use a long random secret
  (`gen-key`) and share the URL only with people who should see the session.
- Ids derive from a fixed application salt, so the same secret yields the same
  public id on every hub: a reused key is linkable across hubs, and a
  precomputed dictionary over weak human-chosen keys works against all
  deployments at once. `--id-salt <ext>` (optional) extends the salt
  per-system, closing both — with `gen-key`'s random keys neither attack
  applies and the flag adds nothing. One value per hub, used by `hub`,
  `gen-key` and `print-id` (`push` doesn't derive ids at all — it sends the
  key, and the hub hashes it). It is not a secret; set it once and keep it —
  changing it invalidates every registered id, which doubles as a deliberate
  mass-revocation lever.
- Client/hub **wire-protocol compatibility is negotiated**, not tied to ids: the
  push client sends the protocol version it speaks, and a hub that can't serve it
  replies with a clear error naming its exact version and which side to upgrade.
  Upgrading either side across a protocol change never rotates session ids or view
  URLs — only a change to the id-derivation scheme itself (rare) does.
- The secret travels in a header on the `/push` WebSocket upgrade. Terminate TLS
  so it isn't sent in the clear. The hub can do this itself:

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

  ACME defaults to Let's Encrypt **staging** (untrusted certs, generous rate
  limits) so you can test the plumbing; add `--acme-production` for real certs,
  and always set `--acme-cache` or the account + certificate are re-issued on
  every restart.
- Behind a TLS-terminating reverse proxy (Traefik, nginx, …) instead: the
  client's `push` URL just needs to be `https://…`, and the hub honors
  `X-Forwarded-Proto`/`X-Forwarded-Host` so the view URL it prints matches the
  proxy's public address. The push is a WebSocket (`/push`), so forward the
  `Upgrade`/`Connection` headers — nginx then tunnels it without buffering.
  Viewer streams (`/s/<id>/events`, SSE) send `X-Accel-Buffering: no`, so nginx
  won't buffer those either. **Mounting under a subpath**
  (`example.com/glass/ → hub`) works: pages reference every asset and stream
  relatively (view pages are canonical at `/s/<slug>/`, the slash-less form
  redirects), so they resolve under whatever prefix the proxy strips. Set
  `X-Forwarded-Prefix` if your proxy doesn't already, so the view URL the hub
  *logs* includes the prefix too.
- The hub trusts allowed clients: it caps a pushed WebSocket message at 16 MB but
  does not otherwise rate-limit the content. A bad-key flood is bounded
  (concurrent auth hashes are capped, each rejection logged for fail2ban), but
  don't expose an open hub to the internet.
- The read-only SSH view authorizes by the **view handle in the username** (the
  slug, which defaults to the session id) — the same read capability as the view
  URL. Treat exposing it like sharing the view URL.

## Status

Everything above works today; the terminal emulation (a vendored vt100 with
local fidelity fixes), inline images, and both renderers are exercised by CI.
Modes are cargo features: the default build is the full multi-call `shellglass`;
`cargo build --no-default-features --features hub` (or `serve`, `push`,
`sessions`) yields a slim build, and per-mode binaries
(`shellglass-{serve,push,hub,sessions,keytool}`) wrap the same CLI.
Not yet: multiple sessions/panes in one view. Not planned: scrollback — the
mirror shows the live screen, like a glance over the operator's shoulder; your
own terminal already has history.

## License

MIT — see [LICENSE](LICENSE).
