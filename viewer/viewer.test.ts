// Unit tests for the browser renderer's pure logic (no DOM needed). Run with
// `node --test` — Node strips the TypeScript types on import.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  palette,
  resolveRgb,
  cellStyle,
  isFillGlyph,
  isCanvasGlyph,
  renderRow,
  patchCells,
  decodeBlock,
  setConfig,
  setProto,
  setReloadPage,
  apply,
  type Cfg,
} from "./viewer.ts";

const CFG: Cfg = {
  defFg: "#d0d0d0",
  defBg: "#000000",
  fillFont: "monospace",
  sym: [],
};
setConfig(CFG);

test("version hello reloads on wire or js mismatch, not on match", () => {
  let reloads = 0;
  setReloadPage(() => {
    reloads += 1;
  });
  setProto(3, "aabbcc");
  apply({ v: 3, js: "aabbcc" } as never);
  assert.equal(reloads, 0, "matching hello is inert");
  apply({ v: 4, js: "aabbcc" } as never);
  assert.equal(reloads, 1, "wire bump reloads");
  apply({ v: 3, js: "ddeeff" } as never);
  assert.equal(reloads, 2, "viewer.js change reloads");
  // A page without boot versions (older HTML) never self-reloads.
  setProto(undefined, undefined);
  apply({ v: 9, js: "zz" } as never);
  assert.equal(reloads, 2);
});

test("palette matches the xterm-256 layout", () => {
  assert.deepEqual(palette(1), [0xcd, 0x00, 0x00]); // base red
  assert.deepEqual(palette(15), [0xff, 0xff, 0xff]); // bright white
  assert.deepEqual(palette(16), [0, 0, 0]); // cube origin
  assert.deepEqual(palette(231), [255, 255, 255]); // cube corner
  assert.deepEqual(palette(232), [8, 8, 8]); // grayscale start
  assert.deepEqual(palette(255), [238, 238, 238]); // grayscale end
});

test("resolveRgb handles the three color forms", () => {
  assert.equal(resolveRgb(null), null);
  assert.equal(resolveRgb(undefined), null);
  assert.deepEqual(resolveRgb(9), [0xff, 0, 0]); // index → palette
  assert.deepEqual(resolveRgb([1, 2, 3]), [1, 2, 3]); // rgb passthrough
});

test("cellStyle emits colors, weight, and reverse video", () => {
  assert.equal(cellStyle({ f: 1, b: true }, false), "color:#cd0000;font-weight:bold;");
  // Inverse on an otherwise-default cell swaps in the default fg/bg.
  assert.equal(cellStyle({ n: true }, false), "color:#000000;background:#d0d0d0;");
  // The cursor reverses too; inverse XOR cursor cancels back to normal.
  assert.equal(cellStyle({ n: true }, true), "");
  assert.equal(cellStyle({}, true), "color:#000000;background:#d0d0d0;");
});

test("cellStyle dim matches the Rust floor formula, italic/underline emit", () => {
  // Rust: f/10*6 (integer division) — default fg 0xd0=208 → 20*6 = 120 = 0x78.
  assert.equal(cellStyle({ d: true }, false), "color:#787878;");
  // On a palette color: bright red 255 → 25*6 = 150 = 0x96.
  assert.equal(cellStyle({ f: 9, d: true }, false), "color:#960000;");
  assert.equal(
    cellStyle({ i: true, u: true }, false),
    "font-style:italic;text-decoration:underline;",
  );
});

test("renderRow coalesces same-style cells into one positioned run", () => {
  const html = renderRow([{ t: "a" }, { t: "b" }, { t: "c" }], -1);
  assert.equal(html, '<span class="run" style="left:0ch;width:3ch;">abc</span>');
});

test("renderRow positions each run absolutely by column", () => {
  // A styled middle cell splits the row into three runs, each at its own column.
  const html = renderRow([{ t: "a" }, { t: "b", b: true }, { t: "c" }], -1);
  assert.match(html, /left:0ch;width:1ch;">a</);
  assert.match(html, /left:1ch;width:1ch;font-weight:bold;">b</);
  assert.match(html, /left:2ch;width:1ch;">c</);
});

test("renderRow marks the cursor cell with reverse video", () => {
  const html = renderRow([{ t: "x" }], 0);
  assert.match(html, /color:#000000;background:#d0d0d0;/);
});

test("wide cells advance two columns", () => {
  // Same-style cells coalesce (as render_row does), so the wide glyph shows up as
  // extra width, not a separate run: 世(2) + a(1) = width 3.
  assert.equal(
    renderRow([{ t: "世", w: true }, { t: "a" }], -1),
    '<span class="run" style="left:0ch;width:3ch;">世a</span>',
  );
  // A style break after the wide glyph reveals the column advance: the next run
  // starts at column 2, not 1.
  const split = renderRow([{ t: "世", w: true }, { t: "a", b: true }], -1);
  assert.match(split, /left:0ch;width:2ch;">世</);
  assert.match(split, /left:2ch;width:1ch;font-weight:bold;">a</);
});

test("box-drawing lines route to the canvas as transparent (selectable) text", () => {
  // │ ─ ┼ etc. are drawn on the overlay canvas; the DOM keeps the real glyph as
  // transparent text (no SVG) so it stays selectable/copyable.
  assert.ok(isCanvasGlyph("│".codePointAt(0)!));
  assert.ok(isCanvasGlyph("┼".codePointAt(0)!));
  const box = renderRow([{ t: "│" }], -1);
  assert.doesNotMatch(box, /<svg/);
  assert.match(box, /color:transparent">│<\/span>/);
  // A block glyph (not a line) is NOT a canvas glyph — still a stretched SVG.
  assert.ok(!isCanvasGlyph("█".codePointAt(0)!));
  assert.match(renderRow([{ t: "█" }], -1), /<svg /);
});

test("block fill runs still merge into one SVG; lines don't (they're canvas)", () => {
  // █ is a mergeable fill glyph and not canvas-routed → one stretched span.
  const bar = renderRow([{ t: "█" }, { t: "█" }, { t: "█" }], -1);
  assert.equal((bar.match(/<svg /g) ?? []).length, 1, "block run not merged");
  assert.match(bar, /left:0ch;width:3ch;/);
  // A line run is canvas-routed: per-cell transparent spans, no SVG.
  const div = renderRow([{ t: "─" }, { t: "─" }, { t: "─" }], -1);
  assert.equal((div.match(/<svg /g) ?? []).length, 0, "line run went to SVG");
  assert.equal((div.match(/color:transparent/g) ?? []).length, 3, "line cells not transparent");
});

test("patchCells writes line patches and reports dirty rows", () => {
  const state = { cells: [[{ t: "a" }, { t: "b" }]], cur: null as [number, number] | null };
  const dirty = patchCells(state, {
    cur: [0, 1],
    rows: [{ r: 0, l: 1, cells: [{ t: "X" }] }],
  });
  assert.equal(state.cells[0][1].t, "X", "patched cell written");
  assert.deepEqual(state.cur, [0, 1], "cursor updated");
  assert.ok(dirty.has(0), "changed + cursor row is dirty");
});

test("patchCells pads a growing row with blanks, never holes", () => {
  // A span starting past the current row end (canonical spaces upstream made
  // the in-between cells "unchanged") must not leave undefined holes.
  const state = { cells: [[{ t: "s" }, { t: "h" }]], cur: null as [number, number] | null };
  patchCells(state, { cur: null, rows: [{ r: 0, l: 4, cells: [{ t: "$" }] }] });
  assert.deepEqual(state.cells[0], [{ t: "s" }, { t: "h" }, { t: " " }, { t: " " }, { t: "$" }]);
  assert.doesNotThrow(() => renderRow(state.cells[0], -1));
});

test("patchCells tri-state cursor: undefined leaves it untouched", () => {
  const state = { cells: [[{ t: "a" }], [{ t: "b" }]], cur: [0, 0] as [number, number] | null };
  // Cursor unchanged (undefined): kept, and its row is NOT dirtied.
  const dirty = patchCells(state, { cur: undefined, rows: [{ r: 1, l: 0, cells: [{ t: "X" }] }] });
  assert.deepEqual(state.cur, [0, 0], "cursor untouched");
  assert.ok(!dirty.has(0), "cursor row not dirtied when unchanged");
  assert.ok(dirty.has(1), "patched row dirty");
  // Explicit null hides it (and dirties the old cursor row).
  const dirty2 = patchCells(state, { cur: null, rows: [] });
  assert.equal(state.cur, null);
  assert.ok(dirty2.has(0), "old cursor row re-rendered on hide");
});

test("patchCells marks both old and new cursor rows dirty", () => {
  const state = { cells: [[{ t: "a" }], [{ t: "b" }]], cur: [0, 0] as [number, number] | null };
  const dirty = patchCells(state, { cur: [1, 0], rows: [] });
  assert.ok(dirty.has(0), "old cursor row");
  assert.ok(dirty.has(1), "new cursor row");
});

test("decodeBlock expands runs, blanks, clusters, and style runs", () => {
  // "aBc" is one merged run — one cell per codepoint; the style run covers B.
  const cells = decodeBlock([["aBc"], [[1, 1, { f: 1, b: 1 }]]]);
  assert.deepEqual(cells, [{ t: "a" }, { t: "B", f: 1, b: 1 }, { t: "c" }]);
  // A blank cell rides as 0 between runs.
  assert.deepEqual(decodeBlock([["a", 0, "b"]]), [{ t: "a" }, { t: "" }, { t: "b" }]);
  // Strings split by CODEPOINT: an astral glyph is one cell, not two UTF-16 units.
  assert.deepEqual(decodeBlock([["x🚀y"]]), [{ t: "x" }, { t: "🚀" }, { t: "y" }]);
  // A multi-codepoint grapheme cell arrives as ["…"] and stays one cell.
  assert.deepEqual(decodeBlock([["a", ["é"], "b"]]), [
    { t: "a" },
    { t: "é" },
    { t: "b" },
  ]);
  // A style run spans multiple cells with one entry.
  assert.deepEqual(decodeBlock([["abcd"], [[0, 3, { d: 1 }]]]), [
    { t: "a", d: 1 },
    { t: "b", d: 1 },
    { t: "c", d: 1 },
    { t: "d" },
  ]);
  // An empty block decodes to no cells.
  assert.deepEqual(decodeBlock([[]]), []);
});

test("uniform spans decode one cell per codepoint with a shared style", () => {
  // Mirror of the applyCell/decodeRow bare-string rule.
  const expand = (text: string, style?: object) => {
    const cells: object[] = [];
    for (const ch of text) cells.push(style ? { t: ch, ...style } : { t: ch });
    return cells;
  };
  assert.deepEqual(expand("✶", { f: 174 }), [{ t: "✶", f: 174 }]);
  assert.deepEqual(expand("ok!", { f: 2 }), [
    { t: "o", f: 2 },
    { t: "k", f: 2 },
    { t: "!", f: 2 },
  ]);
  // Codepoint split: astral glyphs stay whole.
  assert.deepEqual(expand("a🚀"), [{ t: "a" }, { t: "🚀" }]);
});

test("flags as 1 style like true (weight, reverse, dim, wide)", () => {
  assert.equal(cellStyle({ f: 1, b: 1 }, false), "color:#cd0000;font-weight:bold;");
  assert.equal(cellStyle({ n: 1 }, false), "color:#000000;background:#d0d0d0;");
  assert.equal(cellStyle({ d: 1 }, false), "color:#787878;");
  // Wide flag as 1 advances two columns.
  const split = renderRow([{ t: "世", w: 1 }, { t: "a", b: 1 }], -1);
  assert.match(split, /left:2ch;width:1ch;font-weight:bold;">a</);
});
