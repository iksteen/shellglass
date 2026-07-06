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
  u?: Flag; // underline
  n?: Flag; // inverse
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
// flattens its style letters (f,g,b,d,i,u,n,w) into the envelope. The cursor is a
// separate `p` key on every diff-family message.
interface FullMsg {
  d: Block[];
  w: number;
  h: number;
  p?: Cur; // cursor [row, col]; absent = hidden
}
// On diff-family messages the cursor is TRI-STATE: absent = unchanged,
// null = became hidden, [row, col] = moved. A cursor-only move drops `r`,
// leaving just { p }.
interface DiffMsg {
  r?: WireRow[];
  p?: Cur;
}
// A uniform span: c is the bare [row, left, "…"] tuple — ONE CELL PER CODEPOINT
// — and the style flattened into the message applies to every cell.
interface CellMsg extends Style {
  c: [number, number, string];
  p?: Cur;
}
// A single changed line: l is the bare [row, left, entries, runs?] tuple.
interface LineMsg {
  l: WireRow;
  p?: Cur;
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
// server was upgraded under us — reload to fetch the matching page + viewer.js
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
export function setConfig(c: Cfg): void {
  cfg = c;
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
  if (cell.u) s += "text-decoration:underline;";
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
  if (!canvasEl || !obsScreen || !gCols || !gRows) return;
  const rect = obsScreen.getBoundingClientRect();
  if (!rect.width || !rect.height) return;
  cellW = rect.width / gCols;
  cellH = rect.height / gRows;
  dpr = window.devicePixelRatio || 1;
  canvasEl.width = Math.round(rect.width * dpr);
  canvasEl.height = Math.round(rect.height * dpr);
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
      const isCursor = !!screen.cur && screen.cur[0] === r && screen.cur[1] === c;
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

// Emit one SVG symbol span (already-escaped glyph, precomputed boxStyle) covering w
// columns. Fill glyphs (isFillGlyph) stretch to the full box width via textLength so
// lines tile; other symbol_map glyphs scale to fit (xMidYMid meet).
function symbolSpan(
  col: number,
  w: number,
  boxStyle: string,
  font: string,
  glyph: string,
  first: number,
): string {
  const fill = isFillGlyph(first);
  const par = fill ? "none" : "xMidYMid meet";
  // Fill glyphs span the whole box so lines tile; a monospace advance is only ~0.6em,
  // so a bare none-stretch under-fills and horizontals dash. textLength forces the
  // glyph to the viewBox width; none then maps it onto the full box.
  const stretch = fill ? ' textLength="14" lengthAdjust="spacingAndGlyphs"' : "";
  return (
    `<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
    `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
    `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor"${stretch}>${glyph}</text></svg></span>`
  );
}

function symbolCell(cell: Cell, isCursor: boolean, col: number, w: number, font: string): string {
  const boxStyle = cellStyle(cell, isCursor);
  const t = cell.t ?? " ";
  return symbolSpan(col, w, boxStyle, font, esc(t), t.codePointAt(0) ?? 0x20);
}

// ── row rendering (port of render.rs:render_row) ──────────────────────────────

// Render one row's cells to inner HTML. `cursorCol` is the cursor column, or -1.
export function renderRow(cells: Cell[], cursorCol: number): string {
  let out = "";
  let col = 0;
  let runStyle: string | null = null;
  let runCol = 0;
  let cols = 0;
  let text = "";
  const flushText = () => {
    if (text.length === 0) return;
    out += `<span class="run" style="left:${runCol}ch;width:${cols}ch;${runStyle ?? ""}">${text}</span>`;
    text = "";
  };
  for (const cell of cells) {
    const isCursor = col === cursorCol;
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
      cols = 0;
      out += `<span class="run" style="left:${col}ch;width:${w}ch;${cellStyle(cell, isCursor)}color:transparent">${esc(cell.t!)}</span>`;
      col += w;
      continue;
    }
    const font = svgFont(cell);
    if (font) {
      // A symbol_map or long-tail fill glyph: its own scaled-SVG span (per cell — these
      // are rare, so run-merging them isn't worth the bookkeeping).
      flushText();
      runStyle = null;
      cols = 0;
      out += symbolCell(cell, isCursor, col, w, font);
    } else {
      const style = cellStyle(cell, isCursor);
      if (runStyle !== style) {
        flushText();
        runStyle = style;
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
  rowEls: HTMLElement[];
}

let screen: ScreenState = { cells: [], cur: null, rowEls: [] };
let screenEl: HTMLElement;

// Update the screen's cell buffer + cursor from decoded line patches, returning
// the rows to re-render (changed lines plus the old and new cursor rows). The
// cursor is tri-state: undefined = unchanged (leave it, dirty nothing extra),
// null = hidden, [row, col] = moved. DOM-free, so it's unit-tested.
export function patchCells(
  state: { cells: Cell[][]; cur: Cur },
  dp: { cur: Cur | undefined; rows: { r: number; l: number; cells: Cell[] }[] },
): Set<number> {
  const dirty = new Set<number>();
  if (dp.cur !== undefined) {
    if (state.cur) dirty.add(state.cur[0]);
    if (dp.cur) dirty.add(dp.cur[0]);
    state.cur = dp.cur;
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

function applyFull(m: FullMsg): void {
  const cur = m.p ?? null;
  const rows = m.d.map(decodeBlock);
  let html = `<div class="screen" style="width:${m.w}ch;height:calc(${m.h} * var(--lh));">`;
  for (let r = 0; r < rows.length; r++) {
    html += `<div class="row">${renderRow(rows[r], cursorCol(cur, r))}</div>`;
  }
  html += "</div>";
  screenEl.innerHTML = html;

  const screenDiv = screenEl.firstElementChild as HTMLElement;
  screen = {
    cells: rows,
    cur,
    rowEls: Array.from(screenDiv.children) as HTMLElement[],
  };
  // The canvas lives inside .screen (rebuilt each full frame), sized to the grid, and
  // repainted from the fresh cells.
  attachCanvas(m.w, m.h, screenDiv);
  redrawCanvasAll();
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

// `m.c` passes through as-is: undefined = cursor unchanged, null = hidden.
function applyPatches(cur: Cur | undefined, rows: { r: number; l: number; cells: Cell[] }[]): void {
  const dirty = patchCells(screen, { cur, rows });
  for (const r of dirty) {
    const el = screen.rowEls[r];
    if (!el) continue;
    el.innerHTML = renderRow(screen.cells[r] ?? [], cursorCol(screen.cur, r));
    redrawCanvasRow(r);
  }
}

function applyDiff(m: DiffMsg): void {
  applyPatches(m.p, (m.r ?? []).map(decodeRow));
}

function applyCell(m: CellMsg): void {
  const { c: r, p: _p, ...style } = m;
  const styled = Object.keys(style).length > 0;
  const cells: Cell[] = [];
  for (const ch of r[2]) cells.push(styled ? { t: ch, ...style } : { t: ch });
  applyPatches(m.p, [{ r: r[0], l: r[1], cells }]);
}

function applyLine(m: LineMsg): void {
  applyPatches(m.p, [decodeRow(m.l)]);
}

function applyBanner(m: BannerMsg): void {
  screenEl.innerHTML = m.b;
  screen = { cells: [], cur: null, rowEls: [] };
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
  es.onmessage = (e) => apply(JSON.parse(e.data) as Msg);
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

function main(): void {
  const boot = (
    window as unknown as { SHELLGLASS: { events: string; cfg: Cfg; proto?: number; js?: string } }
  ).SHELLGLASS;
  setConfig(boot.cfg);
  setProto(boot.proto, boot.js);
  screenEl = document.getElementById("screen")!;
  connect(boot.events);
}

// Only bootstrap in the browser; importing this module in Node (tests) is inert.
if (typeof document !== "undefined" && (window as unknown as { SHELLGLASS?: unknown }).SHELLGLASS) {
  main();
}
