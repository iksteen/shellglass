# Enhancement roadmap

Ordered plan for the work unlocked by vendoring `crates/vt100` (see the provenance
header in its `Cargo.toml`), plus the cross-layer features that build on it. Every
claim below was verified against the vendored 0.16.2 source at writing time
(2026-07). Work top to bottom within a phase; phases are priority tiers, not
strict dependencies — the only hard edge is called out on the items themselves.

Ground rules that apply throughout:

- **Mirror fidelity is the bar**: render in the browser exactly what the local
  terminal shows — never more, never less. A feature the local terminal doesn't
  render (e.g. OSC 8 on a dumb terminal) must not appear in the mirror.
- **Vendored-crate patches** land as separate commits and are listed in
  `crates/vt100/Cargo.toml`'s provenance header (keep it current).
- **Wire changes**: purely additive optional keys need no salt bump; anything an
  old decoder would *misread* does (see the `SALT` comment in `proto.rs`).

## Phase 1 — quick wins ✅ (all landed 2026-07-09)

Small, self-contained, each in the established SCOSC/SCORC pattern
(`screen.rs`/`perform.rs` + tests in the vendored suite + a consumer-side pin
where it matters). Landed as: telemetry `b6a0a6a`, REP `4c0df56`,
movement + tab stops `4c4e809`.

1. **Unhandled-sequence telemetry.** The parser is constructed with default
   callbacks, so unhandled CSI/escape/OSC vanish silently — which is exactly why
   the SCOSC gap shipped unnoticed. Implement `vt100::Callbacks` in `pty.rs`
   with `unhandled_csi`/`unhandled_escape`/`unhandled_osc`/`unhandled_char`
   recording once per distinct sequence kind (never per-occurrence — a busy TUI
   can emit thousands per second). **Sink constraint**: in serve/push mode we
   own the terminal (raw mode + tee), so NOTHING may be printed while the
   session runs — a stray log line lands inside the mirrored screen. Instead:
   accumulate the (tiny, deduplicated) set in memory and print one summary line
   to stderr on exit, *after* raw mode is restored and the screen thread has
   quiesced ("shellglass: N escape sequences not mirrored: CSI b, OSC 9 —
   please report"). For live debugging of long-running sessions, an env-gated
   file sink (`SHELLGLASS_SEQ_LOG=<path>`, append + flush per new kind) — a
   file, never the tty. Do this FIRST: it converts every remaining gap on this
   page — and future ones — from a user bug report into an exit line.

2. **REP (`CSI b`, repeat preceding graphic character).** Missing from the CSI
   dispatch, and both `xterm-256color` and `xterm-kitty` terminfo advertise
   `rep`, so ncurses ≥ 6 apps (vim, htop, tmux redraws) compress character runs
   into REP today — the mirror silently loses them. Requires remembering the
   last printed char on the screen (cleared on cursor movement/control, per
   xterm). Highest-value single fix: live content loss, a few lines.

3. **Cursor-movement batch.** One-liners each, missing from dispatch: `CSI f`
   (HVP, the CUP alias), `CSI Z` (CBT), `CSI I` (CHT), ``CSI ` `` (HPA),
   `CSI a` (HPR), `CSI e` (VPR). Plus real tab stops: `ESC H` (HTS) and
   `CSI g` (TBC) with a tab-stop table in `grid.rs` — `tab()` currently assumes
   fixed 8-column stops, and CBT/CHT need the table anyway. Wrong cursor
   position is the bug class of the powerline incident; this batch is cheap
   insurance against the rest of the family. One commit.

## Phase 1.5 — direct phase-1 fallout ✅ (landed 2026-07-09: DECSTR `fbbe4a1`, sync output `11a8eb1`)

The very first telemetry runs (item 1) surfaced both of these in trivial test
sessions, so real workloads hit them constantly. Quick-win sized; do before
phase 2.

1. **DECSTR (`CSI ! p`, soft terminal reset).** Small and mechanical: restore
   the defined subset of state without touching screen content or cursor
   position — SGR to normal, scroll margins to full screen, origin mode off,
   autowrap on, insert→replace mode, cursor visible, saved-cursor state
   cleared, keypad/cursor-keys to normal. All of it already exists as state in
   the vendored `Screen`/`Grid`; needs a `Some(b'!')` intermediate branch in
   `csi_dispatch` (only bare and `?` exist). The care is the checklist —
   match xterm's documented reset list exactly, one test assertion per aspect.
   Without it, apps that soft-reset on exit leave the mirror with stale
   margins/attrs the local terminal already dropped.

2. **Synchronized output (`CSI ? 2026 h/l`).** neovim and modern tmux wrap
   redraws in BSU/ESU so the terminal presents atomically; kitty honors it
   locally, so a ≤30fps frame snapshot landing *mid-redraw* shows the browser
   a torn frame the local screen never displayed — a transient but real
   fidelity violation, on every redraw of every 2026-aware app. Vendored side
   is trivial (mode bit in the existing `?h`/`?l` dispatch + a public
   `synchronized_update()` accessor). pty.rs side: the frame publish
   condition additionally holds while the mode is set — keep-showing-the-
   last-frame is exactly the spec's presentation semantics — plus a MANDATORY
   timeout guard (~1s, like real terminals): an app Ctrl-C'd between `h` and
   `l` leaves the mode set forever, the same failure class as an unterminated
   image sequence. With the timeout, worst case degrades to today's behavior.
   No wire/viewer/salt impact for either item.

## Phase 1.6 — telemetry fallout, round 2 ✅ (landed 2026-07-10: vt100 `c21661d`, telemetry `6698ebb`)

A real-workload exit report (2026-07-10) flagged five kinds: `CSI c`, `CSI t`,
`ESC \`, `OSC 10`, `OSC 11`. Verified against the vendored source: four of the
five have **no rendering effect** — they are queries, whose replies are the
*real* terminal's job (it sees the teed output and answers into our stdin→PTY
bridge), or pure string syntax. Only the OSC 10/11 *set* form can change what
the screen looks like — that's item 9 in phase 3; everything else is
telemetry noise to silence deliberately.

1. **No-op arms for non-visual sequences, so telemetry stays high-signal.**
   Deliberately-ignored ≠ unhandled: give these real (empty) dispatch arms in
   the vendored crate so they stop landing in the exit report, each with a
   comment naming why ignoring is faithful:
   - `CSI c` / `CSI > c` (Primary/Secondary DA) — identity queries, zero
     render effect; the local terminal answers via the tee (`probe_caps`
     relies on exactly that reply reaching us, not the parser).
   - `ESC \` (ST) — vte ends an OSC/DCS string itself and then reports the
     terminator as a bare esc_dispatch; pure syntax. (These sightings were
     the terminators of the OSC 10/11 queries below.)
   - `CSI t` (XTWINOPS), every op except the already-handled `8` (resize,
     `Callbacks::resize`): the report ops (11/13/14/16/18/19/21) are queries
     answered by the tee, and title push/pop (22/23) renders nothing while the
     mirror has no title feature — item 12 must un-ignore 22/23 if it lands.
   - `OSC 10;?` / `OSC 11;?` (default-color *queries*) — vim/neovim background
     detection; answered by the tee. The **set** form must NOT be silenced:
     it really changes the local screen, and must keep reporting until item 9
     mirrors it.
   While there: record `CSI t`'s params in the telemetry kind (join them like
   `h`/`l`/`m` in `csi_kind`) so a future unknown op is diagnosable straight
   from the exit line instead of reading as a bare `CSI t`.

## Phase 1.7 — telemetry fallout, round 3 ✅ (landed 2026-07-10: vt100 `95e0058`)

The second exit report (`CSI 4 l`, `CSI ? 7 h/l`, `CSI ? 12 h`, `ESC ( B`)
split differently from round 2: two real gaps, two noise kinds.

1. **DECAWM (`CSI ? 7 h/l`, autowrap)** — real. The parser wrapped
   unconditionally; with autowrap off a real terminal clamps the cursor at the
   right margin and overwrites the edge cell (status bars write the
   bottom-right cell this way), so the mirror showed spilled lines the local
   screen never had. Modeled as an inverted mode bit (zero default = wrap on),
   reset by DECSTR.
2. **IRM (`CSI 4 h/l`, insert mode)** — real. Bare SM/RM had no dispatch at
   all; insert mode now shifts the rest of the row right before each glyph
   lands. Other bare modes keep reporting (params ride the telemetry kind).
   Reset by DECSTR.
3. **Cursor blink (`CSI ? 12 h/l`)** — noise: the mirror renders a steady
   cursor by design (the `cursorDeco` ponytail in viewer.ts); un-ignore if
   blink is ever rendered.
4. **SCS US-ASCII (`ESC ( B` / `ESC ) B`)** — noise: designating the only
   charset the crate models is already true. `ESC ( 0` (DEC line drawing)
   stays loud — if it ever shows up in a report, charset support becomes a
   real roadmap item.

## Phase 2 — good to have

4. **First-class image placements in the grid** ✅ *(the reason the crate was
   vendored — replaces the corner-sentinel machinery)*. Landed 2026-07-10 as
   vt100 `fddf2de` + PTY `76dac5b`, with one deviation from the plan below:
   instead of an id-only cell field plus a grid-owned id→placement table (whose
   positions every scroll/IL/DL path would have to maintain), each stamped cell
   carries `(id, row_off, col_off)` — the offsets make any surviving cell
   reconstruct the top-left exactly, so no table and no scroll hooks exist at
   all. Remaining follow-ups (each small now that ids exist): per-cell holes on
   the wire (`ImagePlacement` erased-cells key + viewer clip), kitty `a=p`
   (id store) and `a=d` deletes, z-ordering. Original plan: `pty.rs` tracked a
   placed image by writing four Plane-16 PUA sentinel glyphs into the vt100 grid
   at the image's corners and reconstructing the top-left each frame
   (`resolve_images`). It worked, but was an approximation with documented
   ponytails: interior overwrites couldn't punch holes in the overlay, kitty
   `a=p`/`a=d` were unimplementable, and the sentinel scan was a per-frame cost.
   Plan:
   - `crates/vt100`: give cells an optional image id (`Cell` grows a
     `Option<ImageId>`-shaped field; keep it `Copy`-friendly — an id, not the
     payload) and the `Grid` an id→placement table (top-left, cell extent).
     Placement stamps the id into the covered cells; vt100's own
     scroll/reflow/erase then manage lifetime *per cell*, exactly mirroring a
     cell-based sixel terminal's erase semantics — corner sampling becomes
     exact-region tracking for free.
   - `parse.rs`/`pty.rs`: on snapshot, derive each image's surviving cell
     region from the stamped cells (bounding box + which cells are gone);
     delete the sentinel/`drop_mark`/`resolve_images` machinery.
   - Wire: unchanged at first (`i` list on full frames, same placements —
     derivation changes, encoding doesn't). Per-cell erase (holes) needs a
     viewer clip mechanism later — ship region tracking first, holes as a
     follow-up (`ImagePlacement` gains an optional erased-cells key: additive,
     no salt bump).
   - Unlocked follow-ups (each small once ids exist): kitty `a=p` (id store —
     the `t`-then-`p` emitters that currently show nothing), `a=d` deletes,
     z-ordering.
   - Test strategy: port the existing sentinel tests (scroll/clip/evict,
     prompt-overwrite survival, wide-glyph columns) to the new mechanism before
     deleting the old one; they encode the erase-semantics contract.

5. **Modern SGR: undercurl, strikethrough, double underline, underline color.**
   ✅ Landed 2026-07-10 as vt100 `96d1630` + wire/viewer `c3d9d02` (also
   dotted/dashed 4:4/4:5, the 58 colon/colorspace form, and the SSH viewer).
   Wire: `u` carries the kitty style number (1 doubles as the legacy flag —
   old decoders degrade to single underline), `s`/`k` are new optional keys;
   no salt bump. Original rationale: the `sgr` match handled underline as
   exact `[4]`, so `4:3` (undercurl —
   helix and neovim diagnostics emit it) falls through and the underline is
   lost entirely; strikethrough (9/29), double underline (21), and underline
   color (58/59) are absent. Cross-layer: vendored `attrs.rs` storage → wire
   style-run flags (additive keys — old viewers ignore, no salt bump) →
   `viewer.ts` rendering (CSS `text-decoration-style: wavy` etc.). Kitty
   renders all of these locally, so the mirror is visibly wrong in editors
   daily. Gate: only emit wire flags the viewer renders.

6. **DECSCUSR (`CSI Sp q`) cursor shape.** ✅ Landed 2026-07-10 as vt100
   `7de444d` + wire/viewers `649dfdf`. Raw 0-6 tracked on `Screen` (DECSTR/RIS
   reset it); wire key `q` (absolute on fulls, two-state on diffs, always sent
   alongside `p` so old decoders parse a style-only change as a cursor no-op —
   no salt bump); browser renders block as reverse video and underline/bar as
   inset box-shadow; SSH viewer passes the sequence through. Blink variants
   render steady (ponytail in `cursorDeco`). Original rationale: not tracked;
   vim's insert-mode beam rendered as a block in the mirror.

## Phase 3 — nice to have

7. **OSC 8 hyperlinks.** ✅ Landed 2026-07-10 as vt100 `ef49e17` +
   wire/viewer `46904b7`, with one deviation from the sketch below: the link
   id rides `Attrs` (OSC 8 is SGR-like state stamped by the print path), not
   an `ImageCell`-pattern cell field — with the guards that matter: SGR 0
   does not close a link, `Cell::clear` strips it (no clickable blanks), and
   ids are URI-deduped on a bounded Screen table so redraws are diff-stable.
   Wire: style key `a` + table key `y` (full table on fulls, additions on
   diffs — which forces the Diff shape), additive, no salt bump. Viewer:
   real `<a target="_blank" rel="noopener noreferrer">` in the DOM path with
   an http/https/ftp/mailto scheme allowlist (a hostile `javascript:` URI
   renders unlinked), attribute-escaped hrefs, injected CSS pinning anchor
   color to terminal styling, hover underline. Storm mode stays linkless, as
   predicted. Original rationale: dropped entirely (no `[b"8", …]` arm); the
   viewer is a browser — real links are the most natural rendering this
   project could offer, and kitty supports OSC 8 locally so fidelity permits
   it.

8. **Damage tracking.** ⊘ Measured out 2026-07-10 — the item's own gate
   ("don't complicate the publish path for a win the numbers don't show")
   closed it. `zz_measure_frame_cost` (SG_FRAMECOST=1, release) puts the full
   extract+encode cost at 84µs/frame at 80×24 and 1.2ms/frame at 320×100,
   flat across typing/scroll/churn — confirming the O(screen-area) thesis,
   but bounding the maximum win at ~3.6% of one core in the worst case
   (320×100 sustained at the 30fps cap; a realistic large-screen typing
   session is <0.5%, idle is zero, and none of it runs on the hub). Against
   that: auditing every vt100 write path for dirty marking, where one missed
   path is a *stale-content correctness bug* in service of a perf-only
   feature. Re-measure before reviving (the meter is one env var away). Two
   cheaper levers first if extraction cost ever matters: extraction dominates
   encode 4–5×, and ~half of it is the per-cell `String` allocation in
   `StyledCell.text` — an inline small-string type cuts that without touching
   any write path; per-row `Arc` sharing in `model::Grid` would let
   `encode_delta` ptr-skip clean rows without vt100 changes.
   Original plan: a dirty-rows bitset maintained by `grid.rs`'s own write
   paths letting both `grid_from_screen` and `encode_delta` skip clean rows.

9. **OSC 10/11 set form: default foreground/background color.** ✅ Landed
   2026-07-10 as vt100 `888b956` + wire/viewers `6e2b120`. Both XParseColor
   shapes parsed (unparseable values keep reporting); wire key `e` on full
   frames (additive, a change forces a full like images, no salt bump);
   browser mutates cfg.defFg/defBg + inline #screen style; SSH viewer passes
   the OSC through. Original rationale: *(the one real gap in the 2026-07-10
   telemetry batch — see phase 1.6 for the noise half)*.
   `OSC 10;<color>` / `OSC 11;<color>` (plus the `OSC 110`/`111`
   resets) change the terminal's default fg/bg; kitty applies them live, so
   after a theme switcher or an `OSC 11`-emitting TUI runs, the local screen
   repaints and the mirror silently keeps its configured colors — visible
   divergence on every default-colored cell. Cross-layer, the item-5 pattern:
   the vendored `Screen` stores the two overrides (parse at least `#RRGGBB`
   and `rgb:RR/GG/BB`; unparseable values stay unhandled so telemetry keeps
   flagging them), the wire ships them as an additive full-frame key (old
   viewers ignore it → no salt bump), and the viewer maps them onto the
   default-color CSS it already derives from the render config. Gate: only
   emit what the viewer renders, and keep the *query* forms in phase 1.6's
   no-op arms — they must not set anything.

13. **Split binaries: the full CLI plus per-mode executables.** ✅ Landed
    2026-07-10 (single commit: lib+cli restructure, features, bins, Docker,
    CI matrix). Feature-matched sizes: full 16.2MB → hub-only 12.9MB (the
    remainder is russh/acme/axum, its real dependencies), key utilities
    0.9MB; the full CLI degrades to the compiled-in subcommands, so the
    Docker image kept its `shellglass hub` entrypoint with just build flags.
    Original rationale: today one
    binary carries every mode; a hub deployment ships the PTY backend, image
    interceptor, font machinery and SSH viewer it never runs (and their
    dependency tree), while a push-only dev box ships the hub. Offer slim
    per-mode binaries — `hub`, `push`, `serve`, and the `gen-key`/`print-id`
    key utilities — alongside the full multi-call binary. Mechanically:
    cargo features gating the mode modules + one `[[bin]]` target per mode
    (each a thin `main` over the same clap actions, so flags/behavior can't
    drift from the full binary), CI building the matrix, and the release
    workflow deciding which artifacts to publish (the Docker hub image is the
    obvious first customer — it only needs `hub`). Watch the bake-in split:
    `viewer.js` + the page template belong to everything that serves viewers
    (serve *and* hub — the hub serves the page and re-serves pushed render
    configs), while the `fonts.rs` discovery machinery (fontdb/fc-match) is
    client-side only — asset baking must follow the features, not the binary
    names.

Dropped from this phase (2026-07-10, deliberate): **scrollback**. The mirror
shows the live screen — a glance over the operator's shoulder — and history is
out of scope by design: the diff protocol is screen-shaped, hub memory stays
bounded, and the operator's own terminal already has scrollback. Don't revive
without a product-level rethink. (The vendored crate's scrollback buffer stays
at 0 in `Parser::new`.)

## Phase 4 — maybe one day

10. **Sixel interception via vte's DCS hooks.** vte routes DCS byte-by-byte
    through `hook`/`put`/`unhook` (vt100 implements none) — streaming, no size
    cap, string-cancellation semantics for free. But kitty APC isn't exposed by
    vte at all, and OSC payloads sit in a fixed 1024-byte buffer, so the
    interceptor must stay for kitty + iTerm2 regardless. Moving only sixel
    would mean two mechanisms where today there is one — do this only if vte
    itself is ever swapped/patched (vte is actively maintained; the vendor
    argument does not apply to it).

11. **Resize reflow.** `set_size` truncates/pads rows and clears wrap flags on
    width change; kitty reflows wrapped lines. The mirror diverges from the
    local screen after SIGWINCH until the next repaint — which fullscreen apps
    do immediately, so it self-heals in practice. Real reflow is the hardest
    item on this page for the smallest visible payoff. Documented divergence;
    revisit only if users actually hit it.

12. **Window title in the viewer.** ✅ Landed 2026-07-10 as vt100 `649e530` +
    wire/viewers `4e0c56d` (bundled behind the item-9 wire work, as planned).
    Title is `Screen` state (OSC 0/2; XTWINOPS 22/23 un-ignored into a bounded
    save/restore stack; RIS wipes); wire key `t` on full/Diff shapes only —
    deliberately never the flattened `c` form, where old viewers spread
    envelope keys into cell objects and `t` would overwrite cell *text* — so
    no salt bump. Browser follows via document.title (boot title restored on
    clear); SSH viewer passes OSC 2 through.

14. **Experiment: a pre-rendered cell matrix in the web client.** ⊘ Run and
    shelved 2026-07-10 on `exp/cell-matrix` (renderer + bench harness + full
    findings in the branch's final commit). Outcome: full DOM fidelity
    achieved, but the success criterion — beat storm's throughput on the
    cmatrix corpus — was not met: tie at 80×24, storm 1.7× faster at 200×60
    and 3.3× at 320×100 (headless Firefox, honest column-rain corpus through
    the production apply() path). Three optimization rounds (span-precise
    dirt from wire patches, per-cell value-equality skips, canvas-scan
    avoidance) established the wall is architectural, not JS: rain dirties
    nearly every row, a dirty row's display list rebuilds wholesale, so the
    browser pays per retained NODE (~cols×rows) while storm's screen is one
    canvas item. Node count, not mutation count, is the cost of full-screen
    animation. A successor experiment would need one-node-per-row text with
    sparse styled overlays (ghost-style). **Successor tested** (2026-07-10,
    `exp/highlight-renderer`): one text node per row + CSS Custom Highlight
    API styling. The skeleton is vindicated — at 1 range/row it holds the
    16ms shaper cap at every size, confirming node count was the wall — but
    Gecko's highlight painting re-resolves O(total ranges) per frame at
    ~65µs/range, so realistic run granularity (~100 ranges/row) lands at
    390-690ms/flush, worse than the matrix. Falsified on today's Firefox by
    the primitive's implementation, not the architecture; revisit if Gecko
    optimizes highlight invalidation. **Final head-to-head** (2026-07-10,
    definitive — with honestly *fragmented* corpora: sticky per-cell trail
    shades for rain, short varied syntax tokens for editor; an earlier
    2-style corpus let run-coalescing hide the fragmentation cliff and
    understated storm's value. Flush ms, headless Firefox, ~16.6 = shaper
    pacing floor, ⚡ = canvas storm engaged):

    | load    | size    | classic+storm | classic no-storm | matrix |
    |---------|---------|---------------|------------------|--------|
    | typing  | all     | ~16.7         | ~16.6            | ~16.6  |
    | editor  | 80×24   | 17.0 ⚡       | 17.0             | 16.5   |
    | editor  | 200×60  | 16.4 ⚡       | 30.2             | 37.7   |
    | editor  | 320×100 | 27.9 ⚡       | 108.5            | 86.3   |
    | rain    | 80×24   | 16.7 ⚡       | 17.0             | 16.0   |
    | rain    | 200×60  | 17.3 ⚡       | 33.7             | 26.1   |
    | rain    | 320×100 | 21.9 ⚡       | 104.5            | 73.9   |

    Conclusions: canvas storm's margin over pure DOM is ~5× at scale on
    fragmented content — the storm architecture is strongly validated.
    Matrix vs classic-without-storm is a split decision (matrix wins
    sparse-churn fragmentation 1.3–1.4× via its per-cell skip; loses
    dense-churn fragmentation where whole spans rewrite; ties when calm).
    With storm retained for everything heavy, the main-renderer choice only
    governs the calm band, where all three tie — no measured configuration
    beats classic + canvas storm. Kept as is. A third strategy — span reuse
    (row reconciliation onto existing elements, `exp/span-reuse`) — measured
    a wash vs classic (2–5%, within noise): parse/create was never the cost;
    dirty rows are charged per node present, style+paint dominates. Three
    strategies, one wall. Original idea: today the
    DOM path rebuilds each dirty row's coalesced spans from scratch, and the
    cmatrix-class escape hatch is storm mode (canvas `fillText`, lower
    fidelity: no symbol_map, no glyph stretch, no links). The experiment: a
    fixed matrix of pre-created cell elements (one node per column, mutated
    in place — textContent/class swaps, never innerHTML) so a full-screen
    update touches no layout/structure, only paint. The hard parts are
    exactly the things the run-coalescing path exists for: double-wide
    characters (a wide glyph must occupy two matrix slots — hide the
    continuation node and let the glyph overflow, or merge slots on demand),
    over-wide fallback glyphs (❯), combining marks, and per-cell style
    without style-attribute churn (class atlas? CSS custom properties?).
    Success criterion: beats storm mode's throughput *at DOM-path fidelity*
    (symbol_map + stretch + links intact) on the cmatrix corpus, measured via
    the `#sg-stats` shaper numbers — otherwise shelve it on an `exp/` branch
    with findings, like `exp/procedural-glyph-geometry`. If it wins outright,
    it can replace both the coalescing renderer *and* storm mode; a partial
    win (faster but fidelity gaps) keeps it as a third mode, which is
    probably not worth carrying.

## Canvas fidelity track

Context: item-14 experiments — DOM is optimization-exhausted, canvas is ~5×
faster under fragmented churn. Goal: canvas at DOM fidelity, offered as a
first-class renderer switchable with DOM (which stays); each mode carries its
own CRT effect.

### A. Storm-mode fidelity (needed while canvas is the escape hatch)

1. ✅ **symbol_map fonts** (landed 2026-07-10): `drawRowStorm` uses
   `svgFont(cell)` for `ctx.font`.
2. ✅ **Fill-glyph stretch** (landed 2026-07-10, `canvas-track-a`): ink box
   (`actualBoundingBox*`) mapped onto the cell rect via save/translate/scale,
   cached per (font, glyph). Verified: fill-row ink coverage identical to DOM.
3. ✅ **Baseline parity** (landed 2026-07-10, `canvas-track-a`): strut
   arithmetic (half-leading + base-font `fontBoundingBoxAscent`), one
   baseline per row. Verified via `viewer/verify.py` (screenshot ink-centroid
   compare): worst text shift 1.00px → 0.02px. The rig stays for the B items.

### B. Canvas as a first-class switchable renderer (each blocks offering it)

1. ✅ **Underline styles + color** (landed 2026-07-10, `canvas-track-a`):
   `drawUnderline` in `drawRowStorm` branches on `cell.u` (2 double bars,
   3 sampled-sine curly, 4 dotted / 5 dashed phase-locked to absolute x),
   color `resolveRgb(cell.k) ?? fg`; strike through the x-height. Verified
   via `verify.py`: decoration rows at ≤0.02px (dotted/dashed 0.11px —
   Firefox dot geometry differs, accepted).
2. ✅ **DECSCUSR shapes** (landed 2026-07-10, `canvas-track-a`): block
   reverse only when `screen.sty <= 2`; 3/4 bottom bar, 5/6 left bar,
   0.14em in the un-reversed fg. Verified: bar-cursor row at 0.00px.
3. ✅ **OSC 8 interaction layer** (landed 2026-07-10, `canvas-track-a`):
   `cellAt` maps pointer x/y to a cell; hover = `cursor:pointer` + underline
   drawn on that link's cells in the row (blanks included); click opens the
   `linkHref`-vetted URI (`noopener,noreferrer`), suppressed while a
   selection is live. Verified: `verify.py` `?mode=links` self-check.
4. ✅ **Ghost layer hardening** (landed 2026-07-10, `canvas-track-a`):
   `ghostText` extracted pure and tested (wide-char single-emit,
   blank→space with trailing blanks kept, multi-codepoint graphemes);
   selection-freeze unchanged; `::selection` was already viewer-injected so
   custom templates were covered.
5. ✅ **Font + DPR lifecycle** (landed 2026-07-10, `canvas-track-a`): DPR
   matchMedia re-arm existed; `reflowGlyphs` (fonts `loadingdone`) now also
   runs `sizeCanvas()` + `redrawCanvasAll()` so a same-advance font swap
   can't leave stale canvas metrics.
6. ✅ **Over-wide fallback glyphs** (landed 2026-07-10, `canvas-track-a`):
   `drawRowStorm` drops the `fillText` maxWidth clamp for
   `glyphOverflowsCell` cells (DOM lets ❯ overflow; canvas now does too).
7. ✅ **Per-mode CRT** (landed 2026-07-10, `canvas-track-a`): discovery —
   the scanline/grille/sweep/vignette overlays and the `#screen` filter are
   blend-mode layers that already composite over the canvas; only the
   text-shadow phosphor is text-bound. Canvas rows re-create exactly that:
   a per-row blurred lighter self-composite in `drawRowStorm` (per-row, not
   per-flush, so undirtied rows aren't re-brightened). Driven by the same
   `#crt` checkbox; the DOM's chromatic fringe stays text-shadow-only.
   Verified: `verify.py` `?mode=crt` self-check.
8. ✅ **The renderer switch** (landed 2026-07-10, `canvas-track-a`):
   `#render` nav toggle (persisted `shellglass-render`, default via
   `?render=canvas`) + `canvasModeOn()` in the viewer — canvas mode pins
   storm on (re-pinned after every DOM rebuild; watchdog exit suppressed),
   DOM mode keeps classic-with-escalation. `#sg-stats` tags pinned mode
   `canvas` vs load escalation `storm`; documented in README. `bench.py`
   (adopted from `exp/span-reuse`, modes dom-nostorm/dom/canvas — canvas
   driven through the real checkbox path) at rain 320x100:
   6.8 / 22.6 / 25.1 fps.

### C. Canvas-mode-only improvements (after B; DOM mode keeps its behavior)

1. **Text over inline images.** In canvas mode, draw image cells behind
   glyphs (decode the `i` list to `ImageBitmap`s, draw before text) so text
   placed over an image wins, like a real cell terminal. DOM mode keeps the
   `<img>` overlay.
2. ✅ **Device-pixel backgrounds** (verified 2026-07-10, `canvas-track-a`):
   already satisfied by the A.3 coordinate restructure — storm bg fills use
   the same `Math.round(c·cellW·dpr)` edges as `drawGlyph`, and adjacent
   cells share the rounded edge by construction. `verify.py` now scans a
   50-cell contiguous bg run for dark seam columns (both modes: none).
3. **Smooth cursor travel** (opt-in): animate the cursor rect between cells
   over ~80ms instead of teleporting; off by default.

## Sequencing note

Items 4 and 7 both hang per-cell metadata off the vendored grid, and both are
landed. They ended up as two *deliberately different* shapes: images are a
region stamped over cells from outside the print path (the generic `Cell<T>`
data slot, stamped by `Screen::place_data` — the tag type itself lives in
shellglass, keeping the crate consumer-agnostic and extractable), while links
are SGR-like state that flows through the print path (`link` on `Attrs`,
resolved through a bounded URI-deduped `Screen` table — a standard terminal
feature, so it stays native). A future per-cell tag should pick whichever
shape matches how the state arrives.
