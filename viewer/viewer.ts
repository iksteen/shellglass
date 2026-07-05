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

function measureMetrics(): void {
  const cs = getComputedStyle(screenEl);
  cellH = parseFloat(cs.getPropertyValue("--lh")) || 17;
  const probe = document.createElement("span");
  probe.textContent = "0".repeat(100);
  probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre";
  screenEl.appendChild(probe);
  cellW = probe.getBoundingClientRect().width / 100 || 8;
  probe.remove();
  dpr = window.devicePixelRatio || 1;
}

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
export function isCanvasGlyph(cp: number): boolean {
  return boxArms(cp) !== null;
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

function attachCanvas(cols: number, rows: number, screenDiv: HTMLElement): void {
  const c = document.createElement("canvas");
  c.width = Math.round(cols * cellW * dpr);
  c.height = Math.round(rows * cellH * dpr);
  c.style.cssText =
    `position:absolute;top:0;left:0;width:${cols * cellW}px;height:${rows * cellH}px;pointer-events:none`;
  screenDiv.appendChild(c);
  canvasEl = c;
  ctx = c.getContext("2d");
}

function lineWidth(weight: number): number {
  const light = Math.max(1, Math.round(dpr));
  return weight === 2 ? 2 * light : light;
}

function drawBoxCell(r: number, c: number, arms: [number, number, number, number], cell: Cell, isCursor: boolean): void {
  if (!ctx) return;
  const [u, rr, d, l] = arms;
  // Rounded cell boundaries: cell (c+1).x0 === cell c.x1, so bars tile exactly.
  const x0 = Math.round(c * cellW * dpr);
  const x1 = Math.round((c + 1) * cellW * dpr);
  const y0 = Math.round(r * cellH * dpr);
  const y1 = Math.round((r + 1) * cellH * dpr);
  const midX = Math.round((x0 + x1) / 2);
  const midY = Math.round((y0 + y1) / 2);
  const vw = lineWidth(Math.max(u, d));
  const hw = lineWidth(Math.max(l, rr));
  const hvw = Math.floor(vw / 2);
  const hhw = Math.floor(hw / 2);
  ctx.fillStyle = hex(cellFg(cell, isCursor));
  if (u || d) {
    const a = u ? y0 : midY - hhw;
    const b = d ? y1 : midY + hhw;
    ctx.fillRect(midX - hvw, a, vw, b - a);
  }
  if (l || rr) {
    const a = l ? x0 : midX - hvw;
    const b = rr ? x1 : midX + hvw;
    ctx.fillRect(a, midY - hhw, b - a, hw);
  }
}

// Redraw one row's band of the canvas from screen.cells (clears then repaints its box
// cells). Self-contained: a cell's bars stay within its own [y0,y1], so a per-row redraw
// never disturbs neighbours — matching the DOM's per-row update.
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
    const arms = cp ? boxArms(cp) : null;
    if (arms) {
      const isCursor = !!screen.cur && screen.cur[0] === r && screen.cur[1] === c;
      drawBoxCell(r, c, arms, cell, isCursor);
    }
    c += w;
  }
}

function redrawCanvasAll(): void {
  if (!ctx || !canvasEl) return;
  ctx.clearRect(0, 0, canvasEl.width, canvasEl.height);
  for (let r = 0; r < screen.cells.length; r++) redrawCanvasRow(r);
}

// ── symbol / fill glyphs (port of render.rs:is_fill_glyph + svg_font) ──────────

export function isFillGlyph(cp: number): boolean {
  return (
    (cp >= 0xe0b0 && cp <= 0xe0d4) || // powerline separators
    (cp >= 0x2500 && cp <= 0x259f) || // box drawing + block elements
    (cp >= 0x1fb00 && cp <= 0x1fbaf) // legacy computing
  );
}

// A fill glyph uniform along x, so a run of it renders as ONE stretched span instead
// of N per-cell boxes — killing the sub-pixel seams that dash a horizontal divider at
// fractional zoom. Solid horizontal strips only; shades/dashed/side-blocks would smear
// if stretched across a run, so they stay per-cell. Mirrors render.rs::is_mergeable_fill.
export function isMergeableFill(cp: number): boolean {
  return (
    cp === 0x2500 || // ─
    cp === 0x2501 || // ━
    cp === 0x2550 || // ═
    cp === 0x2588 || // █ full block
    (cp >= 0x2581 && cp <= 0x2587) || // ▁▂▃▄▅▆▇ lower strips
    cp === 0x2594 // ▔ upper strip
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
// columns. Shared by single symbol cells and merged fill runs — for a merged run, w
// is the run's total width and the one glyph stretches seamlessly across it (no
// per-cell box boundaries to gap at fractional zoom). Mirrors render.rs.
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
interface FillRun {
  col: number;
  width: number;
  t: string; // raw glyph, for run-continuation comparison
  glyph: string; // escaped, for emission
  style: string;
  font: string;
  first: number;
}

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
  // A run of the same mergeable fill glyph, emitted as one stretched span.
  let fill: FillRun | null = null;
  const flushFill = () => {
    if (fill) {
      out += symbolSpan(fill.col, fill.width, fill.style, fill.font, fill.glyph, fill.first);
      fill = null;
    }
  };
  for (const cell of cells) {
    const isCursor = col === cursorCol;
    const w = cell.w ? 2 : 1;
    const cp0 = cell.t ? cell.t.codePointAt(0)! : 0;
    if (cp0 && isCanvasGlyph(cp0)) {
      // The canvas paints the line; keep the real glyph as transparent text so it stays
      // selectable/copyable. Own span (color forced transparent, background retained).
      flushText();
      flushFill();
      runStyle = null;
      cols = 0;
      out += `<span class="run" style="left:${col}ch;width:${w}ch;${cellStyle(cell, isCursor)}color:transparent">${esc(cell.t!)}</span>`;
      col += w;
      continue;
    }
    const font = svgFont(cell);
    if (font) {
      flushText();
      runStyle = null;
      cols = 0;
      const t = cell.t ?? " ";
      const first = t.codePointAt(0) ?? 0x20;
      if (isMergeableFill(first)) {
        const style = cellStyle(cell, isCursor);
        if (fill && fill.t === t && fill.style === style && fill.font === font) {
          fill.width += w;
        } else {
          flushFill();
          fill = { col, width: w, t, glyph: esc(t), style, font, first };
        }
      } else {
        flushFill();
        out += symbolCell(cell, isCursor, col, w, font);
      }
    } else {
      flushFill();
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
  flushFill();
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
function connect(events: string): void {
  const es = new EventSource(events);
  es.onmessage = (e) => apply(JSON.parse(e.data) as Msg);
  es.onerror = () => {
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
  measureMetrics();
  // A served webfont can shift cellW after boot; re-measure and repaint the overlay.
  document.fonts?.ready.then(() => {
    measureMetrics();
    redrawCanvasAll();
  });
  connect(boot.events);
}

// Only bootstrap in the browser; importing this module in Node (tests) is inert.
if (typeof document !== "undefined" && (window as unknown as { SHELLGLASS?: unknown }).SHELLGLASS) {
  main();
}
