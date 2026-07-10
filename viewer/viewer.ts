// shellglass browser renderer — THE renderer; nothing is painted server-side.
//
// Receives the compact cell-diff stream over SSE and renders it to HTML: run
// coalescing with absolute per-run positioning, SVG-scaled symbol glyphs, the
// xterm-256 palette, and reverse/dim/bold/italic/underline styling. It keeps the
// full cell grid in memory so a line diff only re-renders the affected rows. The
// page arrives with an empty #screen and the first SSE event after the version
// hello is always a full frame, so the initial paint lands one round-trip in.
//
// Compiled to viewer.js (see build.rs) and served at /viewer.js; the page injects
// `window.SHELLGLASS = { events, cfg }` before loading this module.

// ── wire types ────────────────────────────────────────────────────────────────

// A color is null (default), a 0-255 palette index, or an [r,g,b] triple.
export type Color = null | number | [number, number, number];

// Flags arrive as 1 (absent = false); all checks are truthiness-based.
type Flag = 0 | 1 | boolean;

export interface Cell {
  t?: string; // text (grapheme); absent = blank
  f?: Color; // fg
  g?: Color; // bg
  b?: Flag; // bold
  d?: Flag; // dim
  i?: Flag; // italic
  u?: Flag | number; // underline style: 1 single, 2 double, 3 curly, 4 dotted, 5 dashed
  s?: Flag; // strikethrough
  k?: Color; // underline color; absent = follow the text color
  n?: Flag; // inverse
  a?: number; // OSC 8 hyperlink id, resolved through the frame's `y` table
  w?: Flag; // wide (two columns)
}

export type Cur = [number, number] | null;

// A cell's style attributes (everything but the text), keyed like Cell. Flags
// arrive as 1 (absent = false); truthiness checks handle both.
export type Style = Omit<Cell, "t">;

// A text entry: a string is one cell per CODEPOINT (consecutive single-codepoint
// glyphs merged — "foo" is three cells), 0 is a blank cell, and a one-element
// array ["…"] is a single cell holding a multi-codepoint grapheme (combining
// marks), which a merged string could not represent unambiguously.
export type TextEntry = string | number | [string];

// A style run over the block's cell indices: [start, len, style].
export type StyleRun = [number, number, Style];

// Columnar cell block, positional: [text] or [text, style-runs].
export type Block = [TextEntry[]] | [TextEntry[], StyleRun[]];

// A changed line, positional. Two forms by the third element's type:
// [row, left, entries, runs?] — a line span; [row, left, "…", {style}?] — a
// single changed cell (the whole string is that cell's grapheme).
type WireRow =
  | [number, number, TextEntry[]]
  | [number, number, TextEntry[], StyleRun[]]
  | [number, number, string]
  | [number, number, string, Style];

// There is no "t" tag: each message type owns one payload key (d/r/c/l/b/v), and
// apply() dispatches on which is present — `c` FIRST, since the single-cell form
// flattens its style letters (f,g,b,d,i,u,s,k,n,w) into the envelope. The cursor
// is a separate `p` key on every diff-family message.
interface FullMsg {
  d: Block[];
  w: number;
  h: number;
  p?: Cur; // cursor [row, col]; absent = hidden
  q?: number; // DECSCUSR cursor style 0-6; absent = 0 (default block)
  e?: [Color, Color]; // OSC 10/11 default fg/bg overrides; absent = none
  t?: string; // window title (OSC 0/2); absent = none set
  y?: Record<number, string>; // OSC 8 link table (id -> URI); absent = empty
  i?: ImageRef[]; // inline images placed on the screen; absent = none
}
// One inline image (iTerm2/kitty) placed at a cell. `m`/`d` build a data: URL;
// `w`/`h` (cols/rows) size it, else it renders at natural pixel size.
export interface ImageRef {
  r: number; // top row (may be negative: the image is clipped above the top edge)
  c: number; // left col
  w?: number; // width in cells
  h?: number; // height in cells (rows)
  m: string; // mime type
  d: string; // base64 image file
}
// On diff-family messages the cursor is TRI-STATE: absent = unchanged,
// null = became hidden, [row, col] = moved. A cursor-only move drops `r`,
// leaving just { p }. The style `q` is two-state: absent = unchanged,
// value = changed-to (0 = back to default).
interface DiffMsg {
  r?: WireRow[];
  p?: Cur;
  q?: number;
  t?: string; // window title, two-state: absent = unchanged ("" = cleared)
  y?: Record<number, string>; // NEW link-table entries, merged in
}
// A uniform span: c is the bare [row, left, "…"] tuple — ONE CELL PER CODEPOINT
// — and the style flattened into the message applies to every cell.
interface CellMsg extends Style {
  c: [number, number, string];
  p?: Cur;
  q?: number;
}
// A single changed line: l is the bare [row, left, entries, runs?] tuple.
interface LineMsg {
  l: WireRow;
  p?: Cur;
  q?: number;
}

// Materialize text entries + style runs into per-cell objects (the form renderRow
// consumes). for..of on a string iterates CODEPOINTS (unlike split(""), which
// would shred surrogate pairs), matching the encoder's merge rule exactly.
export function decodeCells(text: TextEntry[], runs?: StyleRun[]): Cell[] {
  const cells: Cell[] = [];
  for (const v of text) {
    if (typeof v === "number") cells.push({ t: "" });
    else if (typeof v === "string") for (const ch of v) cells.push({ t: ch });
    else cells.push({ t: v[0] });
  }
  for (const [start, len, st] of runs ?? []) {
    for (let i = start; i < start + len && i < cells.length; i++) {
      cells[i] = { t: cells[i].t, ...st };
    }
  }
  return cells;
}

export function decodeBlock(block: Block): Cell[] {
  return decodeCells(block[0] ?? [], block[1]);
}
interface BannerMsg {
  b: string;
}
// Version hello, first event of every SSE stream: the wire proto and the baked
// viewer.js content tag. If either differs from what this page booted with, the
// the server upgraded under us — reload to fetch the matching page + viewer.js
// (guarded against reload storms).
interface VersionMsg {
  v: number;
  js?: string;
}
type Msg = FullMsg | DiffMsg | CellMsg | LineMsg | BannerMsg | VersionMsg;

export interface Cfg {
  defFg: string; // default fg as #rrggbb (for reverse/dim materialization)
  defBg: string;
  fillFont: string; // base font stack for stretch-fill glyphs
  sym: [number, number, string][]; // [lo, hi, family-stack] symbol_map overrides
}

type RGB = [number, number, number];

// ── config ──────────────────────────────────────────────────────────────────

let cfg: Cfg;
// The *configured* defaults, kept so OSC 10/11 overrides (the full frame's `e`
// key) can be applied by mutating cfg.defFg/defBg — every consumer (reverse/dim
// math, storm fills, canvas line colors) reads cfg live — and reverted on reset.
let baseFg = "";
let baseBg = "";
export function setConfig(c: Cfg): void {
  cfg = c;
  baseFg = c.defFg;
  baseBg = c.defBg;
}

// Apply a full frame's default-color overrides ([fg, bg], null = configured
// default; an absent `e` means no overrides). Returns the CSS to put on the
// screen element ("" = revert to the head CSS).
export function applyDefaults(e: [Color, Color] | undefined): { fg: string; bg: string } {
  const fg = resolveRgb(e?.[0]);
  const bg = resolveRgb(e?.[1]);
  cfg.defFg = fg ? hex(fg) : baseFg;
  cfg.defBg = bg ? hex(bg) : baseBg;
  return { fg: fg ? cfg.defFg : "", bg: bg ? cfg.defBg : "" };
}

// The wire-protocol version + viewer.js tag this page booted with
// (window.SHELLGLASS.proto / .js).
let proto: number | undefined;
let jsTag: string | undefined;
export function setProto(p: number | undefined, js?: string): void {
  proto = p;
  jsTag = js;
}

// Overridable for tests; guarded so a misbehaving server can't reload-loop us.
export let reloadPage = (): void => {
  try {
    const last = Number(sessionStorage.getItem("sg-reload") ?? 0);
    if (Date.now() - last < 5000) return;
    sessionStorage.setItem("sg-reload", String(Date.now()));
  } catch (e) {
    /* no sessionStorage: still reload, worst case the server keeps kicking us */
  }
  location.reload();
};
export function setReloadPage(f: () => void): void {
  reloadPage = f;
}

// ── color ─────────────────────────────────────────────────────────────────────

const BASE16: RGB[] = [
  [0x00, 0x00, 0x00], [0xcd, 0x00, 0x00], [0x00, 0xcd, 0x00], [0xcd, 0xcd, 0x00],
  [0x00, 0x00, 0xee], [0xcd, 0x00, 0xcd], [0x00, 0xcd, 0xcd], [0xe5, 0xe5, 0xe5],
  [0x7f, 0x7f, 0x7f], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
  [0x5c, 0x5c, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
];

// xterm 256-color palette (port of render.rs:palette).
export function palette(i: number): RGB {
  if (i < 16) return BASE16[i];
  if (i < 232) {
    const n = i - 16;
    const L = [0, 95, 135, 175, 215, 255];
    return [L[Math.floor(n / 36)], L[Math.floor(n / 6) % 6], L[n % 6]];
  }
  const v = 8 + 10 * (i - 232);
  return [v, v, v];
}

function hex(c: RGB): string {
  return "#" + c.map((x) => x.toString(16).padStart(2, "0")).join("");
}

function parseHex(s: string): RGB {
  return [
    parseInt(s.slice(1, 3), 16),
    parseInt(s.slice(3, 5), 16),
    parseInt(s.slice(5, 7), 16),
  ];
}

export function resolveRgb(c: Color | undefined): RGB | null {
  if (c == null) return null;
  if (typeof c === "number") return palette(c);
  return c;
}

// ── cell → CSS (port of render.rs:cell_box_style) ─────────────────────────────

export function cellStyle(cell: Cell, isCursor: boolean): string {
  let fg = resolveRgb(cell.f);
  let bg = resolveRgb(cell.g);
  // Reverse video (inverse XOR cursor) swaps fg/bg, materializing defaults.
  if (!!cell.n !== isCursor) {
    const f = fg ?? parseHex(cfg.defFg);
    const b = bg ?? parseHex(cfg.defBg);
    fg = b;
    bg = f;
  }
  if (cell.d) {
    const f = fg ?? parseHex(cfg.defFg);
    fg = [Math.floor(f[0] / 10) * 6, Math.floor(f[1] / 10) * 6, Math.floor(f[2] / 10) * 6];
  }
  let s = "";
  if (fg) s += `color:${hex(fg)};`;
  if (bg) s += `background:${hex(bg)};`;
  if (cell.b) s += "font-weight:bold;";
  if (cell.i) s += "font-style:italic;";
  if (cell.u || cell.s) {
    // The text-decoration shorthand: lines, then style (u: 2 double, 3 wavy
    // undercurl, 4 dotted, 5 dashed), then color (absent = currentcolor, which
    // already follows the inverse/dim fg math above). One CSS limitation,
    // accepted: style and color apply to the strikethrough too when both
    // lines are on — kitty draws the strike straight/fg, but the combination
    // is vanishingly rare.
    let d = `${cell.u ? "underline" : ""}${cell.s ? " line-through" : ""}`;
    const us = { 2: "double", 3: "wavy", 4: "dotted", 5: "dashed" }[
      cell.u as number
    ];
    if (us) d += ` ${us}`;
    const k = resolveRgb(cell.k);
    if (k) d += ` ${hex(k)}`;
    s += `text-decoration:${d.trim()};`;
  }
  return s;
}

// ── canvas line overlay (exp) ─────────────────────────────────────────────────
//
// Box-drawing lines/junctions render on one <canvas> laid over #screen, drawn crisp at
// device pixels: adjacent cells share ROUNDED pixel boundaries, so a vertical divider
// tiles across rows with no seam and no font-hinting fight — the thing stretched SVG
// couldn't do. The DOM keeps the real glyph as transparent text, so selection/copy still
// work. Scope: the arms-coverable subset (lines, corners, tees, crosses, half-lines);
// dashes/doubles/arcs/blocks stay on the font path for now.

let cellW = 8;
let cellH = 17;
let dpr = 1;


// Arm weights "urdl" (0 none, 1 light, 2 heavy) for U+2500–257F; "0000" = not
// arms-coverable (dashes/doubles/arcs/diagonals → left to the font path).
const ARMS =
  "0101020210102020" + // 2500 ─ ━ │ ┃
  "0000000000000000" + // 2504-2507 dashes
  "0000000000000000" + // 2508-250B dashes
  "0110021001200220" + // 250C ┌┍┎┏
  "0011001200210022" + // 2510 ┐┑┒┓
  "1100120021002200" + // 2514 └┕┖┗
  "1001100220012002" + // 2518 ┘┙┚┛
  "1110121021101120" + // 251C ├┝┞┟
  "2120221012202220" + // 2520 ┠┡┢┣
  "1011101220111021" + // 2524 ┤┥┦┧
  "2021201210222022" + // 2528 ┨┩┪┫
  "0111011202110212" + // 252C ┬┭┮┯
  "0121012202210222" + // 2530 ┰┱┲┳
  "1101110212011202" + // 2534 ┴┵┶┷
  "2101210222012202" + // 2538 ┸┹┺┻
  "1111111212111212" + // 253C ┼┽┾┿
  "2111112121212112" + // 2540 ╀╁╂╃
  "2211112212212212" + // 2544 ╄╅╆╇
  "1222212222212222" + // 2548 ╈╉╊╋
  "0000000000000000" + // 254C-254F dashes
  "0000000000000000" + // 2550-2553 doubles
  "0000000000000000" + // 2554-2557 doubles
  "0000000000000000" + // 2558-255B doubles
  "0000000000000000" + // 255C-255F doubles
  "0000000000000000" + // 2560-2563 doubles
  "0000000000000000" + // 2564-2567 doubles
  "0000000000000000" + // 2568-256B doubles
  "0000000000000000" + // 256C double, 256D-256F arcs
  "0000000000000000" + // 2570 arc, 2571-2573 diagonals
  "0001100001000010" + // 2574 ╴╵╶╷
  "0002200002000020" + // 2578 ╸╹╺╻
  "0201102001022010"; //  257C ╼╽╾╿

function boxArms(cp: number): [number, number, number, number] | null {
  if (cp < 0x2500 || cp > 0x257f) return null;
  const o = (cp - 0x2500) * 4;
  const u = +ARMS[o];
  const r = +ARMS[o + 1];
  const d = +ARMS[o + 2];
  const l = +ARMS[o + 3];
  return u || r || d || l ? [u, r, d, l] : null;
}
// Codepoints the canvas overlay paints (as crisp device-pixel geometry). Must stay in
// lockstep with glyphOps() — see its comment. The rest of the legacy-computing /
// powerline fill ranges (wedges, seven-segment, rounded/flame separators) stay on the
// stretched-font path via isFillGlyph().
// ponytail: covers the seam-motivated subset (mosaics + thin bars + separators);
// extend the ranges here and in glyphOps together when the long tail earns it.
export function isCanvasGlyph(cp: number): boolean {
  return (
    (cp >= 0x2500 && cp <= 0x259f) || // box drawing + block elements
    (cp >= 0x1fb00 && cp <= 0x1fb3b) || // sextant mosaics
    (cp >= 0x1fb70 && cp <= 0x1fb7b) || // one-eighth vertical/horizontal bars
    (cp >= 0xe0b0 && cp <= 0xe0b3) // powerline triangle separators
  );
}

// The line color for a box cell — fg after inverse/dim (mirrors cellStyle's fg path).
function cellFg(cell: Cell, isCursor: boolean): RGB {
  let fg = resolveRgb(cell.f) ?? parseHex(cfg.defFg);
  if (!!cell.n !== isCursor) fg = resolveRgb(cell.g) ?? parseHex(cfg.defBg);
  if (cell.d) fg = [Math.floor(fg[0] / 10) * 6, Math.floor(fg[1] / 10) * 6, Math.floor(fg[2] / 10) * 6];
  return fg;
}

let canvasEl: HTMLCanvasElement | null = null;
let ctx: CanvasRenderingContext2D | null = null;
let fontPx = 16; // device-pixel font size for storm-mode fillText (set in sizeCanvas)
let fontFam = "monospace";

// ── canvas text metrics (baseline parity + fill-glyph stretch) ────────────────
//
// The DOM aligns every run on the row's STRUT baseline — the base font's
// ascent placed after half-leading — regardless of a span's own font. Canvas
// must do the same: one baseline per row, derived from the base font, or text
// visibly jumps when storm engages. Computed lazily per (font string), reset
// with the caches on resize/font-load (sizeCanvas bumps fontPx into the key).
const fontMetricsCache = new Map<string, { asc: number; desc: number }>();
function strutMetrics(font: string): { asc: number; desc: number } {
  let m = fontMetricsCache.get(font);
  if (!m && ctx) {
    const prev = ctx.font;
    ctx.font = font;
    const tm = ctx.measureText("Mg");
    // fontBoundingBox* is in device px here (the font is sized in device px).
    // Fallback ratios approximate common monospace metrics.
    m = {
      asc: tm.fontBoundingBoxAscent ?? fontPx * 0.8,
      desc: tm.fontBoundingBoxDescent ?? fontPx * 0.25,
    };
    ctx.font = prev;
    fontMetricsCache.set(font, m);
  }
  return m ?? { asc: fontPx * 0.8, desc: fontPx * 0.25 };
}

// The strut baseline for row r (device px): half-leading above the base
// font's content box, then its ascent — the same arithmetic CSS inline
// layout uses to place the line box baseline.
function rowBaseline(r: number): number {
  const m = strutMetrics(`${fontPx}px ${fontFam}`);
  return Math.round(r * cellH * dpr + (cellH * dpr - (m.asc + m.desc)) / 2 + m.asc);
}

// Ink boxes for fill-glyph stretching, cached per (font, glyph).
const inkBoxCache = new Map<string, { l: number; r: number; a: number; d: number }>();
function inkBox(font: string, glyph: string): { l: number; r: number; a: number; d: number } | null {
  const key = `${font}\0${glyph}`;
  let m = inkBoxCache.get(key);
  if (m === undefined && ctx) {
    const prev = ctx.font;
    ctx.font = font;
    const tm = ctx.measureText(glyph);
    ctx.font = prev;
    m = {
      l: tm.actualBoundingBoxLeft,
      r: tm.actualBoundingBoxRight,
      a: tm.actualBoundingBoxAscent,
      d: tm.actualBoundingBoxDescent,
    };
    inkBoxCache.set(key, m);
  }
  if (!m || m.l + m.r <= 0 || m.a + m.d <= 0) return null;
  return m;
}

let obsScreen: HTMLElement | null = null;
let gCols = 0;
let gRows = 0;
let ro: ResizeObserver | null = null;
let dprMedia: MediaQueryList | null = null;

// Derive cell size from .screen's ACTUAL rendered box (width/cols === 1ch, height/rows
// === --lh), so the canvas grid is exact by construction — no probe, no font-load race,
// no accumulating per-column drift. Sizing the backing store resets the canvas, so
// callers redraw after. Re-run on any .screen reflow (webfont load, zoom, DPR) via a
// ResizeObserver.
function sizeCanvas(): void {
  fontMetricsCache.clear();
  inkBoxCache.clear();
  if (!canvasEl || !obsScreen || !gCols || !gRows) return;
  const rect = obsScreen.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  cellW = rect.width / gCols;
  cellH = rect.height / gRows;
  dpr = window.devicePixelRatio || 1;
  canvasEl.width = Math.round(rect.width * dpr);
  canvasEl.height = Math.round(rect.height * dpr);
  // Storm mode draws text on the canvas with the same face the DOM uses; capture it
  // here so the backing-store size and the font stay in lockstep (both dpr-scaled).
  const cs = getComputedStyle(obsScreen);
  fontPx = parseFloat(cs.fontSize) * dpr;
  fontFam = cs.fontFamily;
}

// devicePixelRatio can change with NO .screen resize when a window moves between a
// HiDPI and a regular monitor (an external display on a laptop — common on MacBooks),
// so the ResizeObserver won't fire and the canvas keeps its old backing-store scale and
// blurs. A `(resolution)` media query flips on exactly that change; it pins one ratio,
// so re-arm it for the new value each time it fires.
function onDprChange(): void {
  sizeCanvas();
  redrawCanvasAll();
  watchDpr();
}
function watchDpr(): void {
  if (typeof matchMedia === "undefined") return;
  dprMedia?.removeEventListener("change", onDprChange);
  dprMedia = matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
  dprMedia.addEventListener("change", onDprChange);
}

function attachCanvas(cols: number, rows: number, screenDiv: HTMLElement): void {
  const c = document.createElement("canvas");
  // Overlay .screen exactly; the backing store is sized in sizeCanvas().
  c.style.cssText = "position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none";
  screenDiv.appendChild(c);
  canvasEl = c;
  ctx = c.getContext("2d");
  obsScreen = screenDiv;
  gCols = cols;
  gRows = rows;
  sizeCanvas();
  if (typeof ResizeObserver !== "undefined") {
    if (!ro) ro = new ResizeObserver(() => { sizeCanvas(); redrawCanvasAll(); });
    ro.disconnect();
    ro.observe(screenDiv);
  }
  watchDpr();
}

// Device-pixel rect for cell (r,c). Boundaries are rounded, and cell (c+1).x0 ===
// cell c.x1, so bars/blocks in adjacent cells tile exactly (no seam).
function cellRect(r: number, c: number): [number, number, number, number] {
  return [
    Math.round(c * cellW * dpr),
    Math.round(r * cellH * dpr),
    Math.round((c + 1) * cellW * dpr),
    Math.round((r + 1) * cellH * dpr),
  ];
}

// A drawing primitive in device pixels. glyphOps() returns these — pure and testable
// without a canvas; paintOps() executes them. `light` is the 1-weight line thickness.
export type Op =
  | { t: "rect"; x: number; y: number; w: number; h: number; alpha?: number }
  | { t: "arc"; cx: number; cy: number; rx: number; ry: number; a0: number; a1: number; lw: number }
  | { t: "line"; x0: number; y0: number; x1: number; y1: number; lw: number }
  | { t: "poly"; pts: [number, number][] };

const rectOp = (x: number, y: number, w: number, h: number, alpha?: number): Op =>
  alpha === undefined ? { t: "rect", x, y, w, h } : { t: "rect", x, y, w, h, alpha };

// Rect from fractional cell coordinates (0..1 in both axes), rounded to device pixels
// so adjacent fractions tile exactly. Used by block/sextant/eighth-bar geometry.
function fracRect(x0: number, y0: number, x1: number, y1: number, u0: number, v0: number, u1: number, v1: number, alpha?: number): Op {
  const W = x1 - x0, H = y1 - y0;
  const a = Math.round(x0 + u0 * W), b = Math.round(x0 + u1 * W);
  const c = Math.round(y0 + v0 * H), d = Math.round(y0 + v1 * H);
  return rectOp(a, c, b - a, d - c, alpha);
}

// Line thickness for a weight (1 light, 2 heavy) given the light thickness.
function lw(weight: number, light: number): number {
  return weight === 2 ? 2 * light : light;
}

// Box-drawing arms (lines, corners, tees, crosses, half-lines). Each of the four arms
// gets its OWN weight (so mixed light/heavy junctions like ┿ ╁ ┞ are faithful), extended
// past centre by the crossing bar's half-extent so the junction fills solid.
function armsOps(x0: number, y0: number, x1: number, y1: number, arms: [number, number, number, number], light: number): Op[] {
  const [u, r, d, l] = arms;
  const midX = Math.round((x0 + x1) / 2);
  const midY = Math.round((y0 + y1) / 2);
  const vh = lw(Math.max(u, d), light) >> 1; // half-width of the vertical bar
  const hh = lw(Math.max(l, r), light) >> 1; // half-height of the horizontal bar
  const ops: Op[] = [];
  if (u) { const t = lw(u, light); ops.push(rectOp(midX - (t >> 1), y0, t, midY + hh - y0)); }
  if (d) { const t = lw(d, light); ops.push(rectOp(midX - (t >> 1), midY - hh, t, y1 - (midY - hh))); }
  if (l) { const t = lw(l, light); ops.push(rectOp(x0, midY - (t >> 1), midX + vh - x0, t)); }
  if (r) { const t = lw(r, light); ops.push(rectOp(midX - vh, midY - (t >> 1), x1 - (midX - vh), t)); }
  return ops;
}

function dashesOps(x0: number, y0: number, x1: number, y1: number, cp: number, light: number): Op[] {
  let horiz: boolean, n: number, weight: number;
  if (cp <= 0x250b) {
    const k = cp - 0x2504;
    horiz = (k & 3) < 2;
    n = k < 4 ? 3 : 4;
    weight = k & 1 ? 2 : 1;
  } else {
    const k = cp - 0x254c;
    horiz = k < 2;
    n = 2;
    weight = k & 1 ? 2 : 1;
  }
  const t = lw(weight, light);
  const midX = Math.round((x0 + x1) / 2);
  const midY = Math.round((y0 + y1) / 2);
  const ops: Op[] = [];
  for (let i = 0; i < n; i++) {
    const s0 = (i + 0.2) / n;
    const s1 = (i + 0.8) / n;
    if (horiz) {
      const a = Math.round(x0 + s0 * (x1 - x0));
      const b = Math.round(x0 + s1 * (x1 - x0));
      ops.push(rectOp(a, midY - (t >> 1), b - a, t));
    } else {
      const a = Math.round(y0 + s0 * (y1 - y0));
      const b = Math.round(y0 + s1 * (y1 - y0));
      ops.push(rectOp(midX - (t >> 1), a, t, b - a));
    }
  }
  return ops;
}

// Double lines: light rails at ±offset (vertical at x, horizontal at y); present arms
// run full length, the ╬ centre hole falls out of the rail spacing.
// Per cp 2550-256C: [up, down, left, right, vDouble, hDouble].
const DOUBLES: number[][] = [
  [0, 0, 1, 1, 0, 1], [1, 1, 0, 0, 1, 0], [0, 1, 0, 1, 0, 1], [0, 1, 0, 1, 1, 0],
  [0, 1, 0, 1, 1, 1], [0, 1, 1, 0, 0, 1], [0, 1, 1, 0, 1, 0], [0, 1, 1, 0, 1, 1],
  [1, 0, 0, 1, 0, 1], [1, 0, 0, 1, 1, 0], [1, 0, 0, 1, 1, 1], [1, 0, 1, 0, 0, 1],
  [1, 0, 1, 0, 1, 0], [1, 0, 1, 0, 1, 1], [1, 1, 0, 1, 0, 1], [1, 1, 0, 1, 1, 0],
  [1, 1, 0, 1, 1, 1], [1, 1, 1, 0, 0, 1], [1, 1, 1, 0, 1, 0], [1, 1, 1, 0, 1, 1],
  [0, 1, 1, 1, 0, 1], [0, 1, 1, 1, 1, 0], [0, 1, 1, 1, 1, 1], [1, 0, 1, 1, 0, 1],
  [1, 0, 1, 1, 1, 0], [1, 0, 1, 1, 1, 1], [1, 1, 1, 1, 0, 1], [1, 1, 1, 1, 1, 0],
  [1, 1, 1, 1, 1, 1],
];
function doublesOps(x0: number, y0: number, x1: number, y1: number, cp: number, light: number): Op[] {
  const [u, d, l, r, vd, hd] = DOUBLES[cp - 0x2550];
  const midX = Math.round((x0 + x1) / 2);
  const midY = Math.round((y0 + y1) / 2);
  const t = lw(1, light);
  const h = t >> 1;
  // Rail offset from centre: ~2 line-widths so the gap reads clearly, clamped to fit
  // the (narrow) cell width — the height always has room, and one offset for both axes
  // keeps the gap the same all around a box.
  const off = Math.max(1, Math.min(2 * t, Math.floor((x1 - x0) / 2) - h));
  // A full double (both axes doubled) forms real corners: the OUTER rail reaches the
  // outer corner, the INNER rail stops at the inner crossing rail so it doesn't cross
  // the gap. That only applies with exactly one arm on the crossing axis (a corner);
  // tees/crosses and half-double junctions keep the rails running to centre.
  const dbl = vd && hd;
  const oneH = !!l !== !!r;
  const oneV = !!u !== !!d;
  const hDir = r ? 1 : -1; // which way the single horizontal arm points
  const vDir = d ? 1 : -1;
  const ops: Op[] = [];
  if (u || d) {
    for (const sx of vd ? [-1, 1] : [0]) {
      const xc = midX + sx * off;
      // sx*hDir<0 ⇒ this rail is on the far side of the horizontal arm ⇒ the OUTER rail.
      const a = u ? y0 : dbl && oneH ? midY + (sx * hDir < 0 ? -off : off) - h : midY - (hd ? off : 0) - h;
      const b = d ? y1 : dbl && oneH ? midY + (sx * hDir < 0 ? off : -off) + h : midY + (hd ? off : 0) + h;
      ops.push(rectOp(xc - h, a, t, b - a));
    }
  }
  if (l || r) {
    for (const sy of hd ? [-1, 1] : [0]) {
      const yc = midY + sy * off;
      const a = l ? x0 : dbl && oneV ? midX + (sy * vDir < 0 ? -off : off) - h : midX - (vd ? off : 0) - h;
      const b = r ? x1 : dbl && oneV ? midX + (sy * vDir < 0 ? off : -off) + h : midX + (vd ? off : 0) + h;
      ops.push(rectOp(a, yc - h, b - a, t));
    }
  }
  return ops;
}

// Rounded corners ╭╮╯╰: a quarter ellipse from the far corner. Its tangent points
// must land on the straight arms' centrelines so it meets ─/│ flush. An arm is a
// t-wide rect at midX-(t>>1), so its centre is midX + off — half a pixel past the
// rounded midpoint only when t is odd. Radii are the distance from the corner to
// that centreline (not the raw half-cell), which also snaps to the rounded grid.
function arcOps(x0: number, y0: number, x1: number, y1: number, cp: number, light: number): Op[] {
  const off = (light % 2) / 2;
  const mx = Math.round((x0 + x1) / 2) + off;
  const my = Math.round((y0 + y1) / 2) + off;
  const corners = [[x1, y1], [x0, y1], [x0, y0], [x1, y0]]; // 256D ╭, 256E ╮, 256F ╯, 2570 ╰
  const angles = [
    [Math.PI, 1.5 * Math.PI], [1.5 * Math.PI, 2 * Math.PI],
    [0, 0.5 * Math.PI], [0.5 * Math.PI, Math.PI],
  ];
  const [cx, cy] = corners[cp - 0x256d];
  const [a0, a1] = angles[cp - 0x256d];
  return [{ t: "arc", cx, cy, rx: Math.abs(cx - mx), ry: Math.abs(cy - my), a0, a1, lw: lw(1, light) }];
}

function diagOps(x0: number, y0: number, x1: number, y1: number, cp: number, light: number): Op[] {
  const t = lw(1, light);
  const ops: Op[] = [];
  if (cp !== 0x2572) ops.push({ t: "line", x0, y0: y1, x1, y1: y0, lw: t }); // ╱ (also ╳)
  if (cp !== 0x2571) ops.push({ t: "line", x0, y0, x1, y1, lw: t }); // ╲ (also ╳)
  return ops;
}

// Block elements: solid rects (halves/eighths/quadrants) and alpha shades.
const QUADRANTS = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14]; // 2596-259F: bit0 TL,1 TR,2 BL,3 BR
function blockOps(x0: number, y0: number, x1: number, y1: number, cp: number): Op[] {
  const W = x1 - x0;
  const H = y1 - y0;
  const R = (u0: number, v0: number, u1: number, v1: number, alpha?: number): Op => {
    const a = Math.round(x0 + u0 * W), b = Math.round(x0 + u1 * W);
    const c = Math.round(y0 + v0 * H), d = Math.round(y0 + v1 * H);
    return rectOp(a, c, b - a, d - c, alpha);
  };
  if (cp === 0x2580) return [R(0, 0, 1, 0.5)]; // ▀
  if (cp >= 0x2581 && cp <= 0x2588) return [R(0, 1 - (cp - 0x2580) / 8, 1, 1)]; // ▁-█ lower
  if (cp >= 0x2589 && cp <= 0x258f) return [R(0, 0, (0x2590 - cp) / 8, 1)]; // ▉-▏ left
  if (cp === 0x2590) return [R(0.5, 0, 1, 1)]; // ▐
  if (cp <= 0x2593) return [R(0, 0, 1, 1, (cp - 0x2590) / 4)]; // ░▒▓ → alpha .25/.5/.75
  if (cp === 0x2594) return [R(0, 0, 1, 0.125)]; // ▔
  if (cp === 0x2595) return [R(0.875, 0, 1, 1)]; // ▕
  const m = QUADRANTS[cp - 0x2596];
  const ops: Op[] = [];
  if (m & 1) ops.push(R(0, 0, 0.5, 0.5));
  if (m & 2) ops.push(R(0.5, 0, 1, 0.5));
  if (m & 4) ops.push(R(0, 0.5, 0.5, 1));
  if (m & 8) ops.push(R(0.5, 0.5, 1, 1));
  return ops;
}

// Legacy-computing sextants (U+1FB00–1FB3B): a 2×3 mosaic. Bit i fills cell
// (col i%2, row i/2); numbering matches Unicode (1 TL, 2 TR … 6 BR). The range
// omits the empty/full cell and the two half-column glyphs (masks 0, 21, 42, 63,
// already block elements), so recover the 6-bit mask by skipping 21 and 42.
export function sextantMask(cp: number): number {
  let m = cp - 0x1fb00 + 1;
  if (m >= 21) m += 1;
  if (m >= 42) m += 1;
  return m;
}
function sextantOps(x0: number, y0: number, x1: number, y1: number, cp: number): Op[] {
  const mask = sextantMask(cp);
  const ops: Op[] = [];
  for (let i = 0; i < 6; i++) {
    if (!(mask & (1 << i))) continue;
    const cx = i % 2, cy = (i / 2) | 0;
    ops.push(fracRect(x0, y0, x1, y1, cx / 2, cy / 3, (cx + 1) / 2, (cy + 1) / 3));
  }
  return ops;
}

// One-eighth bars: vertical stripe at column N (1FB70–75, N=2..7) or horizontal
// stripe at row N (1FB76–7B, N=2..7). N=1/N=8 edges are existing block glyphs.
function eighthBarOps(x0: number, y0: number, x1: number, y1: number, cp: number): Op[] {
  if (cp <= 0x1fb75) {
    const n = cp - 0x1fb70 + 2;
    return [fracRect(x0, y0, x1, y1, (n - 1) / 8, 0, n / 8, 1)];
  }
  const n = cp - 0x1fb76 + 2;
  return [fracRect(x0, y0, x1, y1, 0, (n - 1) / 8, 1, n / 8)];
}

// Powerline separators: solid (E0B0 ►, E0B2 ◄) or hollow (E0B1, E0B3) triangles
// spanning the whole cell so they abut the neighbouring segment edge-to-edge.
function powerlineOps(x0: number, y0: number, x1: number, y1: number, cp: number, light: number): Op[] {
  const midY = Math.round((y0 + y1) / 2);
  const right = cp === 0xe0b0 || cp === 0xe0b1; // apex points right
  const ax = right ? x1 : x0;
  const bx = right ? x0 : x1; // vertical base edge
  if (cp === 0xe0b0 || cp === 0xe0b2) {
    // Bleed the base one pixel past the cell edge, away from the apex. The DOM
    // background of the abutting segment rounds its edge independently of the
    // canvas grid, so a flush base leaves a sub-pixel seam; the neighbour on the
    // base side is the triangle's own colour in a powerline prompt, so the
    // overlap is invisible and closes the gap.
    const bb = bx + (right ? -light : light);
    return [{ t: "poly", pts: [[bb, y0], [ax, midY], [bb, y1]] }];
  }
  const t = lw(1, light);
  return [
    { t: "line", x0: bx, y0, x1: ax, y1: midY, lw: t },
    { t: "line", x0: ax, y0: midY, x1: bx, y1, lw: t },
  ];
}

// Pure: the device-pixel ops that render a box-drawing/block/legacy codepoint into the
// cell rect (x0,y0,x1,y1). `light` is the 1-weight thickness. Exported for unit testing.
// Kept in lockstep with isCanvasGlyph(): a codepoint that routes to the canvas but yields
// no ops would render invisibly (transparent DOM text with nothing painted under it).
export function glyphOps(cp: number, x0: number, y0: number, x1: number, y1: number, light: number): Op[] {
  const arms = boxArms(cp);
  if (arms) return armsOps(x0, y0, x1, y1, arms, light);
  if ((cp >= 0x2504 && cp <= 0x250b) || (cp >= 0x254c && cp <= 0x254f)) return dashesOps(x0, y0, x1, y1, cp, light);
  if (cp >= 0x2550 && cp <= 0x256c) return doublesOps(x0, y0, x1, y1, cp, light);
  if (cp >= 0x256d && cp <= 0x2570) return arcOps(x0, y0, x1, y1, cp, light);
  if (cp >= 0x2571 && cp <= 0x2573) return diagOps(x0, y0, x1, y1, cp, light);
  if (cp >= 0x2580 && cp <= 0x259f) return blockOps(x0, y0, x1, y1, cp);
  if (cp >= 0x1fb00 && cp <= 0x1fb3b) return sextantOps(x0, y0, x1, y1, cp);
  if (cp >= 0x1fb70 && cp <= 0x1fb7b) return eighthBarOps(x0, y0, x1, y1, cp);
  if (cp >= 0xe0b0 && cp <= 0xe0b3) return powerlineOps(x0, y0, x1, y1, cp, light);
  return [];
}

function paintOps(g: CanvasRenderingContext2D, color: string, ops: Op[]): void {
  g.fillStyle = color;
  g.strokeStyle = color;
  for (const op of ops) {
    if (op.t === "rect") {
      if (op.alpha !== undefined) {
        const a = g.globalAlpha;
        g.globalAlpha = op.alpha;
        g.fillRect(op.x, op.y, op.w, op.h);
        g.globalAlpha = a;
      } else {
        g.fillRect(op.x, op.y, op.w, op.h);
      }
    } else if (op.t === "arc") {
      g.lineWidth = op.lw;
      g.beginPath();
      g.ellipse(op.cx, op.cy, op.rx, op.ry, 0, op.a0, op.a1);
      g.stroke();
    } else if (op.t === "poly") {
      g.beginPath();
      g.moveTo(op.pts[0][0], op.pts[0][1]);
      for (let i = 1; i < op.pts.length; i++) g.lineTo(op.pts[i][0], op.pts[i][1]);
      g.closePath();
      g.fill();
    } else {
      g.lineWidth = op.lw;
      g.beginPath();
      g.moveTo(op.x0, op.y0);
      g.lineTo(op.x1, op.y1);
      g.stroke();
    }
  }
}

function drawGlyph(r: number, c: number, cp: number, cell: Cell, isCursor: boolean): void {
  if (!ctx) return;
  const [x0, y0, x1, y1] = cellRect(r, c);
  const light = Math.max(1, Math.round(dpr));
  paintOps(ctx, hex(cellFg(cell, isCursor)), glyphOps(cp, x0, y0, x1, y1, light));
}

// Redraw one row's band of the canvas from screen.cells (clears then repaints its box
// cells). Self-contained: a cell's ink stays within its own [y0,y1], so a per-row
// redraw never disturbs neighbours — matching the DOM's per-row update.
function redrawCanvasRow(r: number): void {
  if (!ctx || !canvasEl) return;
  if (storm) return drawRowStorm(r);
  const row = screen.cells[r];
  const y0 = Math.round(r * cellH * dpr);
  const y1 = Math.round((r + 1) * cellH * dpr);
  ctx.clearRect(0, y0, canvasEl.width, y1 - y0);
  if (!row) return;
  let c = 0;
  for (const cell of row) {
    const w = cell.w ? 2 : 1;
    const cp = cell.t ? cell.t.codePointAt(0)! : 0;
    if (cp && isCanvasGlyph(cp)) {
      // Only a block cursor reverses the glyph's ink; underline/bar cursors
      // ride the DOM span's decoration and leave the colors alone.
      const isCursor =
        !!screen.cur && screen.cur[0] === r && screen.cur[1] === c && screen.sty <= 2;
      drawGlyph(r, c, cp, cell, isCursor);
    }
    c += w;
  }
}

function redrawCanvasAll(): void {
  if (!ctx || !canvasEl) return;
  ctx.clearRect(0, 0, canvasEl.width, canvasEl.height);
  for (let r = 0; r < screen.cells.length; r++) redrawCanvasRow(r);
}

// ── storm mode: full-canvas rendering under animation load ───────────────────
//
// When most of the screen changes every frame (cmatrix, `yes`, fast scroll), the DOM
// is the wrong medium: thousands of positioned spans rebuilt per frame cost tens of
// ms of style+layout+paint, and no amount of pacing makes that fast. So under
// sustained near-full-screen change the visible picture moves to the (already
// cell-exact, dpr-aware) canvas overlay — backgrounds, text, crisp glyph geometry —
// no style recalc, near-zero layout, single composited surface, a few ms per full
// frame. The adaptive shaper then sees cheap frames and raises the rate on its own —
// the two mechanisms compose.
//
// The DOM rows stay, as GHOST TEXT: each row is one unstyled text node
// (textContent, no spans, transparent color) kept in sync with the grid. That is
// the degenerate case DOM layout is fast at, and it keeps select/copy working
// through the canvas (which is pointer-events:none): the browser's selection
// highlight paints in the DOM layer and shows through — the canvas clears to
// transparent instead of filling the default bg, so it never occludes it. The CRT
// text-shadow is forced off on ghost rows (shadows have explicit colors and would
// render even for transparent text). On calm the rows repaint fully styled.
//
// Fidelity: same cells, same color math (cellFg / storm bg mirror cellStyle), same
// canvas geometry for box glyphs. Deliberate storm-only approximations, ponytail:
// symbol_map/SVG fill glyphs render with the base font, text baseline is the
// em-box middle, and selection highlight sits under the canvas ink — pixel-nudge
// differences during full-screen animation only.
let storm = false;
let stormHot = 0; // consecutive high-change flushes (enter counter)
let lastStormy = 0; // last time a flush looked stormy (exit-on-calm/idle timestamp)
let stormTimer: ReturnType<typeof setInterval> | null = null;
const STORM_RATIO = 0.5; // a flush touching ≥ half the rows is "stormy"
const STORM_ENTER = 3; // stormy flushes in a row before switching media
const STORM_EXIT_MS = 1200; // this long without a stormy flush ⇒ back to DOM

// The storm bg for a cell — bg after reverse/cursor (mirrors cellStyle's bg path).
function cellBgRgb(cell: Cell, isCursor: boolean): RGB | null {
  if (!!cell.n !== isCursor) return resolveRgb(cell.f) ?? parseHex(cfg.defFg);
  return resolveRgb(cell.g);
}

// Paint one row band entirely on the canvas: clear to transparent (the ghost text
// below is invisible, #screen supplies the backdrop, and a selection highlight must
// show through), per-cell backgrounds, then ink — crisp geometry for canvas glyphs,
// fillText for everything else. maxWidth pins a glyph into its cell box like the
// DOM's .run overflow:hidden does.
// The renderer switch: canvas mode pins storm on — every dirty row paints on
// the canvas, the DOM stays as ghost text + image overlays. Driven by the
// template's #render checkbox when present (change flips live), else by the
// stored choice / ?render=canvas at boot. DOM mode keeps today's behavior:
// classic renderer with storm escalation under load.
let renderBox: HTMLInputElement | null | undefined;
let renderPref: boolean | undefined;
function canvasModeOn(): boolean {
  if (renderBox === undefined) {
    renderBox = document.getElementById("render") as HTMLInputElement | null;
    renderBox?.addEventListener("change", () => {
      if (renderBox?.checked) setStorm(true);
      else if (!selectionActive()) setStorm(false); // else the watchdog exits later
    });
  }
  if (renderBox !== null) return renderBox.checked;
  if (renderPref === undefined) {
    let stored: string | null = null;
    try {
      stored = localStorage.getItem("shellglass-render");
    } catch {
      /* viewer with localStorage disabled */
    }
    renderPref = stored
      ? stored === "on"
      : new URLSearchParams(location.search).get("render") === "canvas";
  }
  return renderPref;
}

// The template's CRT is mostly blend-mode overlays + a #screen filter, which
// composite over the canvas for free; only the text-shadow phosphor bloom is
// text-bound (and forced off on ghost rows). Canvas rows re-create it below in
// drawRowStorm. Any template with a #crt checkbox opts in; none = no CRT.
let crtBox: HTMLInputElement | null | undefined;
function crtOn(): boolean {
  if (crtBox === undefined) {
    crtBox = document.getElementById("crt") as HTMLInputElement | null;
    // repaint canvas rows when the toggle flips (CSS handles the DOM side)
    crtBox?.addEventListener("change", () => redrawCanvasAll());
  }
  return crtBox !== null && crtBox.checked;
}

function drawRowStorm(r: number): void {
  if (!ctx || !canvasEl) return;
  const y0 = Math.round(r * cellH * dpr);
  const y1 = Math.round((r + 1) * cellH * dpr);
  ctx.clearRect(0, y0, canvasEl.width, y1 - y0);
  // Image slices for this band, under the glyphs. Contain-fit anchored
  // top-left (same math as the <img> overlay), one uniform scale s.
  const imgSpans: [number, number][] = [];
  for (const { ref, el } of screenImages) {
    const natW = el.naturalWidth;
    const natH = el.naturalHeight;
    if (!el.complete || !natW || !natH) continue;
    const sc =
      ref.w && ref.h
        ? Math.min((ref.w * cellW * dpr) / natW, (ref.h * cellH * dpr) / natH)
        : dpr; // no cell box: natural CSS-pixel size
    const ix = ref.c * cellW * dpr;
    const iy = ref.r * cellH * dpr;
    const top = Math.max(y0, iy);
    const bot = Math.min(y1, iy + natH * sc);
    if (bot <= top) continue;
    ctx.drawImage(el, 0, (top - iy) / sc, natW, (bot - top) / sc, ix, top, natW * sc, bot - top);
    imgSpans.push([ix, ix + natW * sc]);
  }
  const row = screen.cells[r];
  if (!row) return;
  // Alphabetic at the row's strut baseline — the DOM line box's own
  // arithmetic, so toggling storm produces no vertical shift.
  ctx.textBaseline = "alphabetic";
  const baseY = rowBaseline(r);
  const defBg = cfg.defBg.toLowerCase();
  // Decoration metrics, sized like the DOM's text-decoration: thickness ~6%
  // of the em, underline just below the baseline, strike through the x-height.
  const th = Math.max(1, Math.round(fontPx * 0.06));
  const ulY = baseY + Math.max(th, Math.round(fontPx * 0.065));
  const strikeY = baseY - Math.round(fontPx * 0.36);
  // Underline in the cell's style (kitty numbering), honoring SGR 58 color.
  const drawUnderline = (x0: number, x1: number, style: number, color: string) => {
    if (!ctx) return;
    ctx.fillStyle = color;
    switch (style) {
      case 2: // double
        ctx.fillRect(x0, ulY, x1 - x0, th);
        ctx.fillRect(x0, ulY + 2 * th, x1 - x0, th);
        break;
      case 3: {
        // curly: sampled sine, phase from absolute x so adjacent cells join
        const amp = Math.max(1, Math.round(fontPx * 0.045));
        const period = Math.max(6, Math.round(fontPx * 0.5));
        ctx.strokeStyle = color;
        ctx.lineWidth = th;
        ctx.beginPath();
        const step = Math.max(1, Math.round(dpr));
        for (let x = x0; x <= x1; x += step) {
          const y = ulY + Math.sin((x * 2 * Math.PI) / period) * amp;
          if (x === x0) ctx.moveTo(x, y);
          else ctx.lineTo(x, y);
        }
        ctx.stroke();
        break;
      }
      case 4: // dotted: th-square dots, one per 2th, phase-locked to x
        for (let x = x0 - (x0 % (2 * th)); x < x1; x += 2 * th) {
          if (x >= x0) ctx.fillRect(x, ulY, th, th);
        }
        break;
      case 5: // dashed: 3th on, 2th off, phase-locked to x
        for (let x = x0 - (x0 % (5 * th)); x < x1; x += 5 * th) {
          const lo = Math.max(x, x0);
          const hi = Math.min(x + 3 * th, x1);
          if (hi > lo) ctx.fillRect(lo, ulY, hi - lo, th);
        }
        break;
      default: // single
        ctx.fillRect(x0, ulY, x1 - x0, th);
    }
  };
  // Block cursors reverse the cell (DOM parity); underline/bar cursors draw
  // their shape and leave the colors alone.
  const blocky = screen.sty <= 2;
  let curFont = "";
  let c = 0;
  for (const cell of row) {
    const w = cell.w ? 2 : 1;
    const isCursor = !!screen.cur && screen.cur[0] === r && screen.cur[1] === c;
    const curBlock = isCursor && blocky;
    const x0 = Math.round(c * cellW * dpr);
    const x1 = Math.round((c + w) * cellW * dpr);
    const bg = cellBgRgb(cell, curBlock);
    // Skip fills that match the default bg (apps often set it explicitly — ncurses
    // color pairs): #screen already shows that color, and an opaque fill here would
    // blanket the selection highlight painting in the ghost layer below.
    if (bg && hex(bg) !== defBg) {
      ctx.fillStyle = hex(bg);
      ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
    } else if (
      imgSpans.length &&
      ((cell.t && cell.t !== " ") || bg) &&
      imgSpans.some(([a, b]) => x0 < b && x1 > a)
    ) {
      // A written cell over an image paints its bg like a real cell terminal
      // (the default-bg skip above is only a selection-highlight courtesy).
      // Untouched blank cells keep showing the image — the wire doesn't say
      // written-blank vs never-touched, so blanks stay transparent.
      ctx.fillStyle = defBg;
      ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
    }
    const cp = cell.t ? cell.t.codePointAt(0)! : 0;
    if (cp && isCanvasGlyph(cp) && !(cp >= 0xe000 && symbolFamily(cp))) {
      drawGlyph(r, c, cp, cell, curBlock);
    } else if (cell.t && cell.t !== " ") {
      // symbol_map / fill-glyph cells draw with their mapped family — canvas
      // uses the served webfonts once loaded, same faces as the DOM path.
      const fam = svgFont(cell) ?? fontFam;
      const font = `${cell.i ? "italic " : ""}${cell.b ? "bold " : ""}${fontPx}px ${fam}`;
      if (font !== curFont) {
        ctx.font = font;
        curFont = font;
      }
      const fg = hex(cellFg(cell, curBlock));
      ctx.fillStyle = fg;
      const ink = isFillGlyph(cp) ? inkBox(font, cell.t) : null;
      if (ink !== null) {
        // Fill glyphs tile the cell like the DOM's stretched SVG: map the
        // glyph's ink box onto the exact cell rect, so separators and block
        // fills leave no hairline gaps.
        const sx = (x1 - x0) / (ink.l + ink.r);
        const sy = (y1 - y0) / (ink.a + ink.d);
        ctx.save();
        ctx.translate(x0 + ink.l * sx, y0 + ink.a * sy);
        ctx.scale(sx, sy);
        ctx.fillText(cell.t, 0, 0);
        ctx.restore();
      } else if (cp && glyphOverflowsCell(cell.t, w) && !symbolFamily(cp)) {
        // Over-wide fallback glyphs overflow their cell, like the DOM's
        // own-cell quarantine with overflow:visible — no maxWidth squeeze.
        ctx.fillText(cell.t, x0, baseY);
      } else {
        ctx.fillText(cell.t, x0, baseY, x1 - x0);
      }
      // Decorations, DOM-parity: underline in the cell's style and SGR 58
      // color, strikethrough through the x-height.
      if (cell.u) {
        const ulColor = resolveRgb(cell.k);
        drawUnderline(x0, x1, typeof cell.u === "number" ? cell.u : 1, ulColor ? hex(ulColor) : fg);
        ctx.fillStyle = fg; // drawUnderline may have changed it
      }
      if (cell.s) ctx.fillRect(x0, strikeY, x1 - x0, th);
    }
    // kitty's hover affordance — cell-level, so spaces inside a link underline too
    if (r === hoverRow && cell.a !== undefined && cell.a === hoverA && !cell.u) {
      drawUnderline(x0, x1, 1, hex(cellFg(cell, curBlock)));
    }
    if (isCursor && !blocky) {
      // DECSCUSR underline (3/4) or bar (5/6) cursor, 0.14em like the DOM's
      // inset box-shadow, in the cell's un-reversed fg.
      const cw = Math.max(1, Math.round(fontPx * 0.14));
      ctx.fillStyle = hex(cellFg(cell, false));
      if (screen.sty >= 5) ctx.fillRect(x0, y0, cw, y1 - y0);
      else ctx.fillRect(x0, y1 - cw, x1 - x0, cw);
    }
    c += w;
  }
  if (crtOn()) {
    // Phosphor bloom: blurred lighter self-composite of the row band, glowing
    // in the ink's own color. Per-row, not per-flush over the full canvas —
    // undirtied rows must not be re-brightened every flush.
    ctx.save();
    ctx.globalCompositeOperation = "lighter";
    ctx.globalAlpha = 0.4;
    ctx.filter = `blur(${1.5 * dpr}px)`;
    ctx.drawImage(canvasEl, 0, y0, canvasEl.width, y1 - y0, 0, y0, canvasEl.width, y1 - y0);
    ctx.restore();
  }
}

// The ghost backing for one row: the grid's plain characters as a single text node.
// textContent (not innerHTML) — no parsing, no spans, no styles; wide cells emit
// their grapheme once and monospace CJK advances 2ch, matching the styled path's
// column math, so a selection maps to the picture the canvas paints on top.
// ── OSC 8 links in storm mode ─────────────────────────────────────────────────
//
// The DOM path renders real anchors; storm's ghost text has none, so pointer
// events map back to cells by grid arithmetic. Hover shows the kitty
// affordance (pointer cursor + underline on the link's cells in that row) and
// click opens the same linkHref-vetted URI a DOM anchor would.
let hoverA: number | undefined;
let hoverRow = -1;
function cellAt(ev: MouseEvent): { cell: Cell; r: number } | null {
  if (!obsScreen || !cellW || !cellH) return null;
  const rect = obsScreen.getBoundingClientRect();
  const col = Math.floor((ev.clientX - rect.left) / cellW);
  const r = Math.floor((ev.clientY - rect.top) / cellH);
  const row = screen.cells[r];
  if (!row || col < 0) return null;
  let c = 0;
  for (const cell of row) {
    const w = cell.w ? 2 : 1;
    if (col < c + w) return { cell, r };
    c += w;
  }
  return null;
}
function setHover(a: number | undefined, r: number): void {
  if (a === hoverA && r === hoverRow) return;
  const old = hoverRow;
  hoverA = a;
  hoverRow = r;
  if (obsScreen) obsScreen.style.cursor = a === undefined ? "" : "pointer";
  if (old >= 0) redrawCanvasRow(old);
  if (r >= 0 && r !== old) redrawCanvasRow(r);
}
function onScreenMove(ev: MouseEvent): void {
  if (!storm) return setHover(undefined, -1);
  const hit = cellAt(ev);
  const linked = hit !== null && linkHref(screen.links, hit.cell.a) !== null;
  setHover(linked ? hit.cell.a : undefined, linked ? hit.r : -1);
}
function onScreenClick(ev: MouseEvent): void {
  if (!storm || selectionActive()) return; // a drag-select release is not a click
  const hit = cellAt(ev);
  const uri = hit === null ? null : linkHref(screen.links, hit.cell.a);
  if (uri !== null) window.open(uri, "_blank", "noopener,noreferrer");
}
function attachLinkHandlers(): void {
  screenEl.addEventListener("mousemove", onScreenMove);
  screenEl.addEventListener("mouseleave", () => setHover(undefined, -1));
  screenEl.addEventListener("click", onScreenClick);
}

export function ghostText(row: Cell[]): string {
  let text = "";
  for (const cell of row) text += cell.t && cell.t.length ? cell.t : " ";
  return text;
}
function ghostRow(r: number): void {
  const el = screen.rowEls[r];
  if (!el) return;
  el.textContent = ghostText(screen.cells[r] ?? []);
}

// Replacing a row's text node destroys any selection Range anchored in it — at storm
// rates a drag-selection would die within one flush. So while a selection is live the
// ghost layer FREEZES (the canvas on top keeps painting the true screen); the user
// copies what was on screen when they selected — the only coherent semantics for
// copying out of a running animation. When the selection clears, the next flush
// resyncs every row.
let ghostStale = false;
function selectionActive(): boolean {
  if (typeof getSelection === "undefined") return false;
  const s = getSelection();
  return s !== null && !s.isCollapsed;
}

// Ghost styling lives in a viewer-injected rule (not the served CSS) so it can never
// skew against this file through the hub, which mixes the pusher's CSS with its own
// viewer.js. The explicit ::selection background is the visible highlight — the UA
// default is unreliable over transparent text.
let ghostCss = false;
function ensureGhostCss(): void {
  if (ghostCss || typeof document === "undefined") return;
  ghostCss = true;
  const st = document.createElement("style");
  st.textContent =
    ".row.ghost{color:transparent;text-shadow:none}" +
    ".row.ghost::selection{background:rgba(110,170,255,.4)}";
  document.head.appendChild(st);
}

// Switch media. Entering: ghost every row (uncolorized selectable text) and paint the
// whole grid on canvas (must be complete — the canvas is now the whole picture).
// Leaving: repaint every row's DOM fully styled and give the canvas back to the
// glyph-only pass.
function setStorm(on: boolean): void {
  if (storm === on) return;
  storm = on;
  if (!on) setHover(undefined, -1);
  for (const { el } of screenImages) el.style.visibility = on ? "hidden" : "";
  ensureGhostCss();
  for (const el of screen.rowEls) el.classList.toggle("ghost", on);
  if (on) {
    for (let r = 0; r < screen.cells.length; r++) ghostRow(r);
    redrawCanvasAll();
    // Exit is time-based, not flush-based: when the animation stops, messages stop,
    // flushes stop — a counter would strand us on canvas forever. A watchdog sees
    // "nothing stormy lately" even in total silence.
    lastStormy = clock();
    stormTimer = setInterval(() => {
      // Never exit while a selection is live: the exit path rewrites every row's
      // innerHTML, destroying the very Ranges the frozen ghost layer protects —
      // and the calm-down fires ~1.2s after the animation ends, exactly when the
      // user reaches for Ctrl-C. Storm stays on (the canvas keeps painting); the
      // first tick after the selection clears drops back to DOM.
      if (clock() - lastStormy > STORM_EXIT_MS && !selectionActive() && !canvasModeOn())
        setStorm(false);
    }, 300);
  } else {
    if (stormTimer !== null) clearInterval(stormTimer);
    stormTimer = null;
    stormHot = 0;
    for (let r = 0; r < screen.cells.length; r++) {
      const el = screen.rowEls[r];
      if (el) el.innerHTML = renderRow(screen.cells[r], cursorCol(screen.cur, r), screen.sty, screen.links);
    }
    redrawCanvasAll();
  }
}

// Structural resets (full frame, banner) rebuild the DOM fresh — drop storm state
// without the exit repaint (the rebuild does it).
function stormReset(): void {
  if (stormTimer !== null) clearInterval(stormTimer);
  stormTimer = null;
  storm = false;
  stormHot = 0;
}

// ── symbol / fill glyphs ──────────────────────────────────────────────────────

// Glyphs that take the stretched-SVG font path: a monospace advance under-fills the
// cell, so the glyph is forced to the full box width (textLength). The box/block/
// sextant/eighth-bar ranges are painted on the canvas (isCanvasGlyph, checked first in
// renderRow) and never reach here; what's left is the legacy-computing long tail
// (wedges, seven-segment, …) and the rounded/flame powerline separators — plus any
// symbol_map glyph that happens to fall in these ranges.
export function isFillGlyph(cp: number): boolean {
  return (
    (cp >= 0xe0b0 && cp <= 0xe0d4) || // powerline separators (E0B0–B3 canvas unless symbol_mapped)
    (cp >= 0x1fb00 && cp <= 0x1fbaf) // legacy computing (sextants + eighth bars are canvas)
  );
}

function symbolFamily(cp: number): string | null {
  for (const [lo, hi, fam] of cfg.sym) {
    if (cp >= lo && cp <= hi) return fam;
  }
  return null;
}

// A glyph whose advance exceeds its cell (w columns) would, inside a coalesced run,
// shove every later glyph rightward — off its column and, at the end, under the
// cursor block (e.g. the prompt `❯` U+276F falls back to a non-monospace face at
// ~1.67ch and eats the character after it). We detect that by measuring against the
// base `0` advance and pin such glyphs to their own scaled cell. ASCII is always 1ch
// in the monospace base, so it's skipped; results are memoised per grapheme, and the
// measuring context is lazily built from #screen's resolved font.
//
// CRITICAL: the @font-face web fonts load async, so an early measurement can see the
// system fallback (❯ at 1ch) and cache the wrong answer for good — the glyph then
// renders wide once the real font swaps in but stays inline, eating the next char.
// (Chrome tends to have fonts by first frame; Firefox's FOUT reliably poisons it.)
// resetGlyphMeasure() clears the cache + context; main() calls it on the fonts
// loadingdone event so the re-render measures against the loaded fonts.
let measCtx: CanvasRenderingContext2D | null = null;
let measOneCh = 0;
const overflowCache = new Map<string, boolean>();
function resetGlyphMeasure(): void {
  measCtx = null;
  measOneCh = 0;
  overflowCache.clear();
}
function glyphOverflowsCell(t: string, w: number): boolean {
  if (typeof document === "undefined") return false;
  const cp = t.codePointAt(0) ?? 0;
  if (cp >= 0x20 && cp <= 0x7e) return false; // printable ASCII: guaranteed 1ch
  const cached = overflowCache.get(t);
  if (cached !== undefined) return cached;
  if (!measCtx) {
    measCtx = document.createElement("canvas").getContext("2d");
    if (!measCtx) return false;
    const cs = getComputedStyle(screenEl);
    measCtx.font = `${cs.fontSize} ${cs.fontFamily}`;
    measOneCh = measCtx.measureText("0").width || 1;
  }
  // 5% slack so ordinary rounding doesn't route normal glyphs down the SVG path.
  const over = measCtx.measureText(t).width > measOneCh * w * 1.05;
  overflowCache.set(t, over);
  return over;
}

// The font stack to render `cell` as a scaled SVG glyph, or null for plain text.
function svgFont(cell: Cell): string | null {
  const t = cell.t ?? "";
  if (!t) return null;
  const cp = t.codePointAt(0)!;
  const fam = symbolFamily(cp);
  if (fam) return fam;
  return isFillGlyph(cp) ? cfg.fillFont : null;
}

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

// Attribute-context escape (href): quotes too.
function escAttr(s: string): string {
  return esc(s).replace(/"/g, "&quot;");
}

// OSC 8 comes from whatever program runs in the mirrored session, so treat the
// URI as hostile: only schemes that can't execute in the page are rendered as
// anchors (javascript:/data:/vbscript: would be viewer XSS one click away).
// file: is allowed — `ls --hyperlink` emits it for every entry, and while the
// browser itself refuses file: navigation from web content, the hover
// affordance and copyable href still mirror what the terminal shows.
export function linkHref(links: Record<number, string>, id: number | undefined): string | null {
  if (id === undefined) return null;
  const uri = links[id];
  if (!uri) return null; // pruned table entry: render unlinked
  return /^(https?|ftp|mailto|file):/i.test(uri) ? uri : null;
}

// Emit one SVG symbol span (already-escaped glyph, precomputed boxStyle) covering w
// columns. When `stretch`, the glyph is forced to the full box (none + textLength) so
// fill glyphs tile and an over-wide glyph is squeezed to 1ch at full height; otherwise
// it scales to fit centred (xMidYMid meet), which keeps symbol_map glyphs' geometry.
function symbolSpan(
  col: number,
  w: number,
  boxStyle: string,
  font: string,
  glyph: string,
  stretch: boolean,
): string {
  const par = stretch ? "none" : "xMidYMid meet";
  // textLength forces the glyph advance to the viewBox width; `none` then maps that onto
  // the full box. Without it a ~0.6em monospace advance under-fills and horizontals dash.
  const len = stretch ? ' textLength="14" lengthAdjust="spacingAndGlyphs"' : "";
  return (
    `<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
    `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
    `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor"${len}>${glyph}</text></svg></span>`
  );
}

function symbolCell(cell: Cell, isCursor: boolean, col: number, w: number, font: string, deco = ""): string {
  const boxStyle = cellStyle(cell, isCursor) + deco;
  const t = cell.t ?? " ";
  const cp = t.codePointAt(0) ?? 0x20;
  // Fill glyphs tile the box (stretch); symbol_map glyphs keep intrinsic geometry (meet).
  // An isFillGlyph that is also symbol-mapped still tiles.
  const stretch = isFillGlyph(cp);
  return symbolSpan(col, w, boxStyle, font, esc(t), stretch);
}

// ── row rendering (port of render.rs:render_row) ──────────────────────────────

// True when a style string paints nothing for a space: no background, no
// underline, no cursor decoration.
function inkFree(s: string): boolean {
  return !s.includes("background") && !s.includes("text-decoration") && !s.includes("box-shadow");
}

// The non-block DECSCUSR cursors (3/4 underline, 5/6 bar) draw as an inset
// box-shadow — no layout impact, currentColor follows the cell's fg. Blink
// variants render steady.
// ponytail: no blink — a CSS animation class needs an injected stylesheet;
// add one if anyone misses it.
function cursorDeco(sty: number): string {
  return sty >= 5
    ? "box-shadow:inset 0.14em 0 0 0 currentColor;"
    : "box-shadow:inset 0 -0.14em 0 0 currentColor;";
}

// Render one row's cells to inner HTML. `cursorCol` is the cursor column (or -1),
// `curSty` the DECSCUSR style: 0-2 render as the classic reverse-video block,
// 3-6 leave the cell's colors alone and add an underline/bar decoration.
// `links` is the OSC 8 id→URI table: linked runs render as real anchors.
export function renderRow(
  cells: Cell[],
  cursorCol: number,
  curSty = 0,
  links: Record<number, string> = {},
): string {
  const blocky = curSty <= 2;
  let out = "";
  let col = 0;
  let runStyle: string | null = null;
  let runHref: string | null = null;
  let runCol = 0;
  let cols = 0;
  let text = "";
  const flushText = () => {
    if (text.length === 0) return;
    const st = `left:${runCol}ch;width:${cols}ch;${runStyle ?? ""}`;
    // A linked run is a real anchor. rel severs the referrer/opener; the
    // scheme was allowlisted in linkHref. Color/decoration are inherited via
    // the injected #screen a.run rule, so page-template `a` styling can't
    // repaint terminal text.
    out +=
      runHref === null
        ? `<span class="run" style="${st}">${text}</span>`
        : `<a class="run" href="${escAttr(runHref)}" target="_blank" rel="noopener noreferrer" style="${st}">${text}</a>`;
    text = "";
  };
  for (const cell of cells) {
    const isCursor = col === cursorCol;
    const curBlock = isCursor && blocky;
    const deco = isCursor && !blocky ? cursorDeco(curSty) : "";
    const w = cell.w ? 2 : 1;
    const cp0 = cell.t ? cell.t.codePointAt(0)! : 0;
    // Canvas paints box/block/legacy geometry, always winning over symbol_map there. The
    // one exception is the PUA powerline arrows (E0B0–B3): a user who symbol_maps them to a
    // Nerd Font did so deliberately, so a mapping hit defers to the font path. The cp>=0xe000
    // guard short-circuits before symbolFamily() on every standard box glyph — the hot path.
    if (cp0 && isCanvasGlyph(cp0) && !(cp0 >= 0xe000 && symbolFamily(cp0))) {
      // The canvas paints the line; keep the real glyph as transparent text so it stays
      // selectable/copyable. Own span (color forced transparent, background retained).
      flushText();
      runStyle = null;
      runHref = null;
      cols = 0;
      out += `<span class="run" style="left:${col}ch;width:${w}ch;${cellStyle(cell, curBlock)}${deco}color:transparent">${esc(cell.t!)}</span>`;
      col += w;
      continue;
    }
    // A glyph whose fallback advance exceeds its cell (❯ U+276F at ~1.67ch) would shove
    // the rest of its coalesced run rightward — off-column and, at the end, under the
    // cursor. Give it its own cell rendered as plain text at natural size, overflowing
    // visibly into the (near-always blank) next cell rather than distorting it into 1ch:
    // every run is absolutely positioned, so neighbours keep their columns regardless.
    if (cp0 && glyphOverflowsCell(cell.t!, w) && !isFillGlyph(cp0) && !symbolFamily(cp0)) {
      flushText();
      runStyle = null;
      runHref = null;
      cols = 0;
      out += `<span class="run" style="left:${col}ch;width:${w}ch;overflow:visible;${cellStyle(cell, curBlock)}${deco}">${esc(cell.t!)}</span>`;
      col += w;
      continue;
    }
    const font = svgFont(cell);
    if (font) {
      // A symbol_map or long-tail fill glyph: its own scaled-SVG span (per cell — these
      // are rare, so run-merging them isn't worth the bookkeeping).
      // ponytail: canvas-glyph/overflow/symbol cells stay plain spans — a
      // linked box-drawing or symbol_map glyph is fringe; the run resets so
      // the anchor never bleeds across.
      flushText();
      runStyle = null;
      runHref = null;
      cols = 0;
      out += symbolCell(cell, curBlock, col, w, font, deco);
    } else {
      let style = cellStyle(cell, curBlock) + deco;
      const href = linkHref(links, cell.a);
      // A blank cell with no visible ink (no bg, no underline) renders identically
      // under any fg/weight — let it ride the open run instead of splitting it, as
      // long as that run is equally ink-free (adopting a bg/underline run would
      // paint the gap) and NOT a link (a blank must never become clickable).
      // Halves the span count on sparse animated screens (cmatrix).
      if (
        (!cell.t || cell.t === " ") &&
        href === null &&
        runHref === null &&
        runStyle !== null &&
        runStyle !== style &&
        inkFree(style) &&
        inkFree(runStyle)
      ) {
        style = runStyle;
      }
      if (runStyle !== style || runHref !== href) {
        flushText();
        runStyle = style;
        runHref = href;
        cols = 0;
      }
      if (cols === 0) runCol = col;
      text += esc(cell.t && cell.t.length ? cell.t : " ");
      cols += w;
    }
    col += w;
  }
  flushText();
  return out;
}

function cursorCol(cur: Cur, row: number): number {
  return cur && cur[0] === row ? cur[1] : -1;
}

// ── screen state + message application ────────────────────────────────────────

interface ScreenState {
  cells: Cell[][];
  cur: Cur;
  sty: number; // DECSCUSR cursor style 0-6
  links: Record<number, string>; // OSC 8 id -> URI
  rowEls: HTMLElement[];
}

let screen: ScreenState = { cells: [], cur: null, sty: 0, links: {}, rowEls: [] };
// Inline images: DOM mode shows them as <img> overlays above the grid; storm
// hides the overlays and draws the same (already decoded) elements onto the
// canvas UNDER the glyphs, so text painted over an image wins — like a real
// cell terminal. Rebuilt on every full frame (images ride only fulls).
let screenImages: { ref: ImageRef; el: HTMLImageElement }[] = [];
let screenEl: HTMLElement;

// Update the screen's cell buffer + cursor from decoded line patches, returning
// the rows to re-render (changed lines plus the old and new cursor rows). The
// cursor is tri-state: undefined = unchanged (leave it, dirty nothing extra),
// null = hidden, [row, col] = moved. DOM-free, so it's unit-tested.
export function patchCells(
  state: { cells: Cell[][]; cur: Cur; sty?: number },
  dp: {
    cur: Cur | undefined;
    sty?: number;
    rows: { r: number; l: number; cells: Cell[] }[];
  },
): Set<number> {
  const dirty = new Set<number>();
  if (dp.cur !== undefined) {
    if (state.cur) dirty.add(state.cur[0]);
    if (dp.cur) dirty.add(dp.cur[0]);
    state.cur = dp.cur;
  }
  if (dp.sty !== undefined && dp.sty !== (state.sty ?? 0)) {
    state.sty = dp.sty;
    if (state.cur) dirty.add(state.cur[0]); // repaint the cursor's shape
  }
  for (const patch of dp.rows) {
    let row = state.cells[patch.r];
    if (!row) {
      row = [];
      state.cells[patch.r] = row;
    }
    for (let dx = 0; dx < patch.cells.length; dx++) {
      const i = patch.l + dx;
      // Pad a growing row with canonical blanks — bare assignment past the end
      // would leave holes (undefined cells) that renderRow can't iterate.
      while (row.length < i) row.push({ t: " " });
      row[i] = patch.cells[dx];
    }
    dirty.add(patch.r);
  }
  return dirty;
}

// Paint is decoupled from apply. A message updates the in-memory `screen` model
// synchronously (cheap), marks what changed, and schedules one rAF flush; the flush
// does the expensive DOM/canvas work once per frame. cmatrix-class output pushes
// ~30fps of near-full-screen diffs; the browser buffers several SSE events while the
// main thread is busy and, without this, would run their innerHTML+canvas paints
// back-to-back and never yield — pinning the thread at ~10fps. Coalescing collapses
// every message queued between two rAFs into a single repaint of the union of dirty
// rows (or one full rebuild) and yields the thread between frames. A hidden tab
// (rAF paused) still applies state but skips all paint — dirtyRows is a Set of row
// indices, so it can't grow past the row count.
let paintScheduled = false;
const dirtyRows = new Set<number>();
let rebuildDims: { w: number; h: number; i?: ImageRef[] } | null = null;
let rebuildBanner: string | null = null;
let lastFlush = 0;

// Adaptive frame shaping. A full-screen 30fps repaint (cmatrix, `yes`, a scrolling
// build log) rebuilds thousands of positioned spans per frame; in a real (GPU) browser
// one such frame costs tens of ms of layout+paint+composite, so painting every server
// frame pins the main thread at 100% and the tab stops responding. Rather than a fixed
// fps cap, we *measure* each paint's real cost (start-of-paint → the next animation
// frame, which the browser fires only once the previous frame's layout/paint/composite
// has committed) and pace the next paint so painting occupies at most TARGET_LOAD of
// wall-clock, leaving the rest for input + message decode. Cheap frames (interactive
// typing) measure ~one vsync and aren't throttled; expensive frames stretch the
// interval out — traffic-shaping on frame cost, self-tuning as the cost rises or falls.
// We always render the *latest* coalesced state and drop the intermediate animation
// frames, the same "show latest, skip ticks" the server's MIN_FRAME does to the PTY;
// the screen always converges to the true current state.
const TARGET_LOAD = 0.7; // spend ≤70% of wall-clock painting
// ponytail: if a single frame measures slower than MAX_INTERVAL×LOAD, we cap the pacing
// interval here (≈4fps floor) and knowingly run over budget rather than stall to
// multi-second gaps — a backstop for a pathologically slow client, not the normal path.
const MAX_INTERVAL = 250;
let paintCost = 16; // EWMA of measured frame cost (ms); seeds at one 60fps frame

// Footer stats counters (see startStats): total SSE payload received, and the number
// of paints actually committed (throttled re-arms don't count).
let bytesIn = 0;
let paints = 0;

const raf = (cb: () => void) =>
  (typeof requestAnimationFrame !== "undefined" ? requestAnimationFrame : (f: () => void) => setTimeout(f, 16))(cb);
const clock = () => (typeof performance !== "undefined" ? performance.now() : 0);

function schedulePaint(): void {
  if (paintScheduled) return;
  paintScheduled = true;
  raf(flushPaint);
}

function flushPaint(): void {
  // A structural rebuild (full frame / banner) always paints now — rare, and it resets
  // everything. Per-row diffs are shaped: if the pacing interval hasn't elapsed, re-arm
  // and coalesce more into the next paint (dirtyRows/cursor persist, stays scheduled).
  const now = clock();
  const interval = Math.min(paintCost / TARGET_LOAD, MAX_INTERVAL);
  if (!rebuildDims && rebuildBanner === null && now - lastFlush < interval) {
    raf(flushPaint);
    return;
  }
  lastFlush = now;
  paintScheduled = false;
  paints++;

  if (rebuildBanner !== null) {
    stormReset(); // banner replaces the grid wholesale
    screenEl.innerHTML = rebuildBanner;
    rebuildBanner = null;
    rebuildDims = null;
    dirtyRows.clear();
    return; // a one-off banner isn't representative steady paint cost — don't sample it
  }
  const t0 = clock();
  if (rebuildDims) {
    stormReset(); // paintFull rebuilds the DOM fresh — start over in DOM mode
    paintFull(rebuildDims);
    rebuildDims = null;
    dirtyRows.clear();
    if (canvasModeOn()) setStorm(true); // canvas mode: re-pin after the rebuild
  } else {
    // Storm detection: a flush touching most rows, several times in a row, means
    // full-screen animation — flip to canvas rendering (see storm mode above).
    const stormy = dirtyRows.size >= STORM_RATIO * (screen.cells.length || 1);
    if (stormy) {
      lastStormy = now;
      if (stormEnabled && !storm && ++stormHot >= STORM_ENTER) setStorm(true); // paints all rows
    } else if (!storm) {
      stormHot = 0;
    }
    const frozen = storm && selectionActive();
    if (storm && !frozen && ghostStale) {
      // Selection just cleared — the ghost layer froze while it was live; resync all.
      for (let r = 0; r < screen.cells.length; r++) ghostRow(r);
      ghostStale = false;
    }
    for (const r of dirtyRows) {
      if (storm) {
        drawRowStorm(r);
        if (frozen) ghostStale = true;
        else ghostRow(r); // keep the selectable backing text in sync with the picture
      } else {
        const el = screen.rowEls[r];
        if (!el) continue;
        el.innerHTML = renderRow(screen.cells[r] ?? [], cursorCol(screen.cur, r), screen.sty, screen.links);
        redrawCanvasRow(r);
      }
    }
    dirtyRows.clear();
  }
  // Sample this frame's true cost once the browser commits it (the next animation frame
  // fires only after layout+paint+composite) and fold it into the EWMA (α=0.3).
  raf(() => {
    paintCost += 0.3 * (clock() - t0 - paintCost);
  });
}

function applyFull(m: FullMsg): void {
  // Update the model now so diffs queued behind this full patch the right cells;
  // the DOM rebuild is deferred to the flush (which reads screen.cells).
  screen = {
    cells: m.d.map(decodeBlock),
    cur: m.p ?? null,
    sty: m.q ?? 0,
    links: m.y ?? {},
    rowEls: [],
  };
  // Default-color overrides apply now (cfg feeds every style computation);
  // the element CSS lands with the rebuild below. Fulls carry the title
  // absolutely: absent = none set.
  defaultsCss = applyDefaults(m.e);
  setTitle(m.t ?? "");
  rebuildDims = { w: m.w, h: m.h, i: m.i };
  rebuildBanner = null;
  dirtyRows.clear(); // a full frame supersedes any pending per-row dirt
  schedulePaint();
}

// The screen element's inline color/background override ("" = the head CSS).
let defaultsCss = { fg: "", bg: "" };

// The page title the document booted with; the session's OSC 0/2 title
// replaces it while set and it comes back when the title is cleared.
let bootTitle: string | null = null;
function setTitle(t: string): void {
  if (typeof document === "undefined") return; // unit tests run DOM-free
  if (bootTitle === null) bootTitle = document.title;
  document.title = t || bootTitle;
}

function paintFull(dims: { w: number; h: number; i?: ImageRef[] }): void {
  // OSC 10/11 overrides: inline style beats the config-derived head CSS;
  // clearing it reverts.
  screenEl.style.color = defaultsCss.fg;
  screenEl.style.backgroundColor = defaultsCss.bg;
  const cur = screen.cur;
  let html = `<div class="screen" style="width:${dims.w}ch;height:calc(${dims.h} * var(--lh));">`;
  for (let r = 0; r < screen.cells.length; r++) {
    html += `<div class="row">${renderRow(screen.cells[r], cursorCol(cur, r), screen.sty, screen.links)}</div>`;
  }
  html += "</div>";
  screenEl.innerHTML = html;

  const screenDiv = screenEl.firstElementChild as HTMLElement;
  screen.rowEls = Array.from(screenDiv.children) as HTMLElement[];
  // The canvas lives inside .screen (rebuilt each full frame), sized to the grid, and
  // repainted from the fresh cells.
  attachCanvas(dims.w, dims.h, screenDiv);
  redrawCanvasAll();

  // Inline images overlay the grid. They ride only in full frames (an image
  // add/remove/move forces one server-side), so rebuilding them here is authoritative;
  // diffs never touch them. Appended after the canvas ⇒ they stack on top.
  if (dims.i?.length) screenDiv.insertAdjacentHTML("beforeend", renderImages(dims.i));
  screenImages = (dims.i ?? []).map((ref, idx) => ({
    ref,
    el: screenDiv.querySelectorAll("img.inline-img")[idx] as HTMLImageElement,
  }));
  for (const { el } of screenImages) {
    el.style.visibility = storm ? "hidden" : "";
    // data: URLs still decode async — a static screen would never repaint, so
    // redraw the canvas when a not-yet-ready image lands.
    if (!el.complete)
      el.addEventListener("load", () => {
        if (storm) redrawCanvasAll();
      });
  }
}

// `<img>` overlays positioned at their cell. Given cols/rows, the image is fit into
// that cell box preserving its own aspect (`contain`, anchored top-left) rather than
// stretched — the emitter (e.g. chafa) sizes the cell box for the *local* terminal's
// cell ratio, which needn't match the browser's, so stretching would distort.
// Without a size it renders at natural pixel size.
function renderImages(imgs: ImageRef[]): string {
  return imgs
    .map((im) => {
      const size = im.w && im.h ? `width:${im.w}ch;height:calc(${im.h} * var(--lh));object-fit:contain;object-position:left top;` : "";
      return `<img class="inline-img" alt="" src="data:${im.m};base64,${im.d}" style="position:absolute;left:${im.c}ch;top:calc(${im.r} * var(--lh));${size}z-index:3;pointer-events:none;">`;
    })
    .join("");
}

function decodeRow([r, l, text, style]: WireRow): { r: number; l: number; cells: Cell[] } {
  if (typeof text === "string") {
    // Bare string = one cell per codepoint; the single style covers all.
    const st = style as Style | undefined;
    const cells: Cell[] = [];
    for (const ch of text) cells.push(st ? { t: ch, ...st } : { t: ch });
    return { r, l, cells };
  }
  return { r, l, cells: decodeCells(text, style as StyleRun[] | undefined) };
}

// The cursor (`m.p`) passes through as-is: undefined = unchanged, null = hidden;
// same for its style (`m.q`): undefined = unchanged.
function applyPatches(
  cur: Cur | undefined,
  sty: number | undefined,
  rows: { r: number; l: number; cells: Cell[] }[],
): void {
  const dirty = patchCells(screen, { cur, sty, rows });
  for (const r of dirty) dirtyRows.add(r);
  schedulePaint();
}

function applyDiff(m: DiffMsg): void {
  if (m.t !== undefined) setTitle(m.t); // two-state: absent = unchanged
  // New link-table entries merge; the next full frame prunes scrolled-off ids.
  if (m.y) Object.assign(screen.links, m.y);
  applyPatches(m.p, m.q, (m.r ?? []).map(decodeRow));
}

function applyCell(m: CellMsg): void {
  const { c: r, p: _p, q: _q, ...style } = m;
  const styled = Object.keys(style).length > 0;
  const cells: Cell[] = [];
  for (const ch of r[2]) cells.push(styled ? { t: ch, ...style } : { t: ch });
  applyPatches(m.p, m.q, [{ r: r[0], l: r[1], cells }]);
}

function applyLine(m: LineMsg): void {
  applyPatches(m.p, m.q, [decodeRow(m.l)]);
}

function applyBanner(m: BannerMsg): void {
  screen = { cells: [], cur: null, sty: 0, links: {}, rowEls: [] };
  rebuildBanner = m.b;
  rebuildDims = null;
  dirtyRows.clear();
  schedulePaint();
}

// Tag-free dispatch on which payload key is present. `c` (cell) MUST come first —
// its flattened style letters (b/d/w) would otherwise read as banner/full/wide.
// A message with only `p` is a cursor-only diff.
export function apply(m: Msg): void {
  if ("v" in m) {
    const wireChanged = proto !== undefined && m.v !== proto;
    const jsChanged = jsTag !== undefined && m.js !== undefined && m.js !== jsTag;
    if (wireChanged || jsChanged) reloadPage();
    return;
  }
  if ("c" in m) applyCell(m);
  else if ("l" in m) applyLine(m);
  else if ("d" in m) applyFull(m);
  else if ("b" in m) applyBanner(m);
  else applyDiff(m); // { r?, p? } — includes cursor-only { p }
}

// EventSource only auto-retries network blips (readyState CONNECTING); on an HTTP
// error — e.g. the hub restarted and 404s the session until its client
// re-registers — it CLOSEs permanently and the page would go dead. Rebuild it on
// CLOSED with a fixed retry; the server sends a full frame on every (re)connect,
// so no client state needs resetting. The last screen stays frozen meanwhile.
// ponytail: fixed 2s retry, no backoff — it's one idle HTTP request per tick.
// Two independent "not live" sources drive the page chrome: the SSE link to the
// server, and — in hub mode — the operator (push source) behind it, reported via a
// named `operator` event (1/0). We publish the combined state as
// body[data-offline="hub"|"operator"] (absent = live); the default template pulses
// its header orb red and drops a big sized-to-fit label over the terminal. A lost
// SSE link ("HUB OFFLINE") outranks a gone operator — if we can't reach the server,
// its last operator status is stale. Harmless for a custom template that ignores
// the attribute, and for standalone (no `operator` events, so a dead source lands
// as the SSE link dropping).
let sseDown = false;
let operatorDown = false;
function refreshLive(): void {
  const state = sseDown ? "hub" : operatorDown ? "operator" : "";
  if (state) document.body.dataset.offline = state;
  else delete document.body.dataset.offline;
}

function connect(events: string): void {
  const es = new EventSource(events);
  es.onopen = () => {
    sseDown = false;
    refreshLive();
  };
  es.onmessage = (e) => {
    bytesIn += (e.data as string).length;
    apply(JSON.parse(e.data) as Msg);
  };
  es.addEventListener("operator", (e) => {
    operatorDown = (e as MessageEvent).data === "0";
    refreshLive();
  });
  es.onerror = () => {
    sseDown = true;
    refreshLive();
    if (es.readyState === EventSource.CLOSED) {
      setTimeout(() => connect(events), 2000);
    }
  };
}

function fmtRate(bytesPerSec: number): string {
  if (bytesPerSec >= 1e6) return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
  if (bytesPerSec >= 1e3) return `${(bytesPerSec / 1e3).toFixed(0)} KB/s`;
  return `${bytesPerSec.toFixed(0)} B/s`;
}

// Footer stats, refreshed once a second: SSE payload throughput, the frames-per-second
// the adaptive shaper is currently allowing (its pacing interval), and the fps actually
// committed. Rates are per-window (deltas ÷ elapsed), so they reflect the last second,
// not a since-boot average. No-ops if the template has no #sg-stats (custom templates).
function startStats(): void {
  const el = document.getElementById("sg-stats");
  if (!el) return;
  let lastBytes = 0;
  let lastPaints = 0;
  let lastT = clock();
  setInterval(() => {
    const t = clock();
    const dt = (t - lastT) / 1000 || 1;
    const bps = (bytesIn - lastBytes) / dt;
    const fps = (paints - lastPaints) / dt;
    const cap = 1000 / Math.min(paintCost / TARGET_LOAD, MAX_INTERVAL);
    lastBytes = bytesIn;
    lastPaints = paints;
    lastT = t;
    el.textContent = `${fmtRate(bps)} · ${fps.toFixed(0)} fps (cap ${cap.toFixed(0)})${storm ? (canvasModeOn() ? " · canvas" : " · storm") : ""}`;
  }, 1000);
}

// Viewer-owned CSS (anchor styling), injected at boot.
function injectViewerCss(): void {
  const linkCss = document.createElement("style");
  linkCss.textContent =
    "#screen a.run{color:inherit;text-decoration:none}" +
    "#screen a.run:hover{text-decoration:underline}";
  document.head.appendChild(linkCss);
}

// ── canvas-track verification hooks (verify.html, bench.html; no SSE) ─────────
let stormEnabled = true; // bench-only knob: pin the classic path for comparison
export function benchInit(el: HTMLElement, mode?: string): void {
  screenEl = el;
  stormEnabled = mode !== "dom-nostorm";
  injectViewerCss();
  attachLinkHandlers();
}
export function benchStats(): { paints: number; cost: number; storm: boolean } {
  return { paints, cost: paintCost, storm };
}
export function benchStorm(on: boolean): void {
  setStorm(on);
}
export function benchFlush(): void {
  flushPaint();
}

function main(): void {
  const boot = (
    window as unknown as { SHELLGLASS: { events: string; cfg: Cfg; proto?: number; js?: string } }
  ).SHELLGLASS;
  setConfig(boot.cfg);
  setProto(boot.proto, boot.js);
  screenEl = document.getElementById("screen")!;
  // OSC 8 anchors: inherit the terminal styling (a page template's own `a`
  // rules must not repaint terminal text) and underline on hover, like kitty.
  injectViewerCss();
  attachLinkHandlers();
  connect(boot.events);
  startStats();
  // Web fonts load async; any glyph-width measured before they land is cached wrong
  // (see resetGlyphMeasure). Drop the cache and re-render every row when fonts arrive so
  // over-wide glyphs (❯) get pinned to their own cell instead of eating the next char.
  const reflowGlyphs = (): void => {
    resetGlyphMeasure();
    // Canvas metrics can go stale even when .screen's box doesn't move (a
    // same-advance font-face swap never fires the ResizeObserver), so re-derive
    // them and repaint the canvas alongside the DOM rows.
    sizeCanvas();
    redrawCanvasAll();
    for (let r = 0; r < screen.cells.length; r++) dirtyRows.add(r);
    schedulePaint();
  };
  // `loadingdone` (not `ready`): @font-face faces load lazily — nothing uses Noto until
  // the first row renders, so document.fonts.ready resolves BEFORE the face starts
  // loading and a one-shot `.then` re-measures while still in FOUT. `loadingdone` fires
  // when a face actually finishes — exactly when re-measuring ❯ becomes correct. `ready`
  // stays as a fallback for the case where the face was already cached before we listen.
  document.fonts?.addEventListener("loadingdone", reflowGlyphs);
  document.fonts?.ready.then(reflowGlyphs);
}

// Only bootstrap in the browser; importing this module in Node (tests) is inert.
if (typeof document !== "undefined" && (window as unknown as { SHELLGLASS?: unknown }).SHELLGLASS) {
  main();
}
