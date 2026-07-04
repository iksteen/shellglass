// Unit tests for the browser renderer's pure logic (no DOM needed). Run with
// `node --test` — Node strips the TypeScript types on import.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  palette,
  resolveRgb,
  cellStyle,
  isFillGlyph,
  renderRow,
  patchCells,
  decodeBlock,
  setConfig,
  type Cfg,
} from "./viewer.ts";

const CFG: Cfg = {
  defFg: "#d0d0d0",
  defBg: "#000000",
  fillFont: "monospace",
  sym: [],
};
setConfig(CFG);

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

test("fill glyphs render as stretched SVG, plain text does not", () => {
  assert.ok(isFillGlyph("│".codePointAt(0)!));
  const box = renderRow([{ t: "│" }], -1);
  assert.match(box, /<svg /);
  assert.match(box, /preserveAspectRatio="none"/);
  const plain = renderRow([{ t: "a" }], -1);
  assert.doesNotMatch(plain, /<svg/);
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

test("flags as 1 style like true (weight, reverse, dim, wide)", () => {
  assert.equal(cellStyle({ f: 1, b: 1 }, false), "color:#cd0000;font-weight:bold;");
  assert.equal(cellStyle({ n: 1 }, false), "color:#000000;background:#d0d0d0;");
  assert.equal(cellStyle({ d: 1 }, false), "color:#787878;");
  // Wide flag as 1 advances two columns.
  const split = renderRow([{ t: "世", w: 1 }, { t: "a", b: 1 }], -1);
  assert.match(split, /left:2ch;width:1ch;font-weight:bold;">a</);
});
