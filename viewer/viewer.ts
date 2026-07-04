// shellglass browser renderer.
//
// Receives the compact cell-diff stream over SSE and renders it to HTML, mirroring
// the Rust reference (`src/render.rs`) cell-for-cell: run coalescing with absolute
// per-run positioning, SVG-scaled symbol glyphs, the xterm-256 palette, and
// reverse/dim/bold/italic/underline styling. It keeps the full cell grid in memory
// so a rectangle diff only needs to re-render the affected rows.
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

interface FullMsg {
  t: "f";
  w: number;
  h: number;
  c?: Cur; // cursor [row, col]; absent = hidden
  r: Block[];
}
// On diff-family messages the cursor is TRI-STATE: absent = unchanged,
// null = became hidden, [row, col] = moved. Empty r is omitted (cursor-only).
interface DiffMsg {
  t: "d";
  c?: Cur;
  r?: WireRow[];
}
// A uniform span: r is the bare [row, left, "…"] tuple — ONE CELL PER CODEPOINT
// — and the style flattened into the message applies to every cell.
interface CellMsg extends Style {
  t: "c";
  c?: Cur;
  r: [number, number, string];
}
// A single changed line: r is the bare [row, left, entries, runs?] tuple.
interface LineMsg {
  t: "l";
  c?: Cur;
  r: WireRow;
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
  t: "b";
  html: string;
}
type Msg = FullMsg | DiffMsg | CellMsg | LineMsg | BannerMsg;

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

// ── symbol / fill glyphs (port of render.rs:is_fill_glyph + svg_font) ──────────

export function isFillGlyph(cp: number): boolean {
  return (
    (cp >= 0xe0b0 && cp <= 0xe0d4) || // powerline separators
    (cp >= 0x2500 && cp <= 0x259f) || // box drawing + block elements
    (cp >= 0x1fb00 && cp <= 0x1fbaf) // legacy computing
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

function symbolCell(cell: Cell, isCursor: boolean, col: number, w: number, font: string): string {
  const boxStyle = cellStyle(cell, isCursor);
  const t = cell.t ?? " ";
  const first = t.codePointAt(0) ?? 0x20;
  const par = isFillGlyph(first) ? "none" : "xMidYMid meet";
  return (
    `<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
    `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
    `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor">${esc(t)}</text></svg></span>`
  );
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
  const flush = () => {
    if (text.length === 0) return;
    out += `<span class="run" style="left:${runCol}ch;width:${cols}ch;${runStyle ?? ""}">${text}</span>`;
    text = "";
  };
  for (const cell of cells) {
    const isCursor = col === cursorCol;
    const w = cell.w ? 2 : 1;
    const font = svgFont(cell);
    if (font) {
      flush();
      runStyle = null;
      cols = 0;
      out += symbolCell(cell, isCursor, col, w, font);
    } else {
      const style = cellStyle(cell, isCursor);
      if (runStyle !== style) {
        flush();
        runStyle = style;
        cols = 0;
      }
      if (cols === 0) runCol = col;
      text += esc(cell.t && cell.t.length ? cell.t : " ");
      cols += w;
    }
    col += w;
  }
  flush();
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
  const cur = m.c ?? null;
  const rows = m.r.map(decodeBlock);
  let html = `<div class="screen" style="width:${m.w}ch;height:calc(${m.h} * var(--lh));">`;
  for (let r = 0; r < rows.length; r++) {
    html += `<div class="row">${renderRow(rows[r], cursorCol(cur, r))}</div>`;
  }
  html += "</div>";
  screenEl.innerHTML = html;

  const screenDiv = screenEl.firstElementChild!;
  screen = {
    cells: rows,
    cur,
    rowEls: Array.from(screenDiv.children) as HTMLElement[],
  };
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
  }
}

function applyDiff(m: DiffMsg): void {
  applyPatches(m.c, (m.r ?? []).map(decodeRow));
}

function applyCell(m: CellMsg): void {
  const { t: _t, c: _c, r, ...style } = m;
  const styled = Object.keys(style).length > 0;
  const cells: Cell[] = [];
  for (const ch of r[2]) cells.push(styled ? { t: ch, ...style } : { t: ch });
  applyPatches(m.c, [{ r: r[0], l: r[1], cells }]);
}

function applyLine(m: LineMsg): void {
  applyPatches(m.c, [decodeRow(m.r)]);
}

function applyBanner(m: BannerMsg): void {
  screenEl.innerHTML = m.html;
  screen = { cells: [], cur: null, rowEls: [] };
}

export function apply(m: Msg): void {
  if (m.t === "f") applyFull(m);
  else if (m.t === "d") applyDiff(m);
  else if (m.t === "c") applyCell(m);
  else if (m.t === "l") applyLine(m);
  else applyBanner(m);
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
  const boot = (window as unknown as { SHELLGLASS: { events: string; cfg: Cfg } }).SHELLGLASS;
  setConfig(boot.cfg);
  screenEl = document.getElementById("screen")!;
  connect(boot.events);
}

// Only bootstrap in the browser; importing this module in Node (tests) is inert.
if (typeof document !== "undefined" && (window as unknown as { SHELLGLASS?: unknown }).SHELLGLASS) {
  main();
}
