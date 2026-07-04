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

test("patchCells writes rect cells and reports dirty rows", () => {
  const state = { cells: [[{ t: "a" }, { t: "b" }]], cur: null as [number, number] | null };
  const dirty = patchCells(state, {
    cur: [0, 1],
    rects: [{ top: 0, left: 1, w: 1, h: 1, cells: [{ t: "X" }] }],
  });
  assert.equal(state.cells[0][1].t, "X", "rect cell written");
  assert.deepEqual(state.cur, [0, 1], "cursor updated");
  assert.ok(dirty.has(0), "changed + cursor row is dirty");
});

test("patchCells marks both old and new cursor rows dirty", () => {
  const state = { cells: [[{ t: "a" }], [{ t: "b" }]], cur: [0, 0] as [number, number] | null };
  const dirty = patchCells(state, { cur: [1, 0], rects: [] });
  assert.ok(dirty.has(0), "old cursor row");
  assert.ok(dirty.has(1), "new cursor row");
});

test("decodeBlock materializes columnar text + sparse style", () => {
  // "a" plain, "B" bold+red (idx 1), "c" plain — only index 1 has a style entry.
  const cells = decodeBlock({ t: ["a", "B", "c"], s: { 1: { f: 1, b: true } } });
  assert.deepEqual(cells, [{ t: "a" }, { t: "B", f: 1, b: true }, { t: "c" }]);
  // A blank cell rides as 0 and decodes to empty text (rendered as a space).
  assert.deepEqual(decodeBlock({ t: ["a", 0] }), [{ t: "a" }, { t: "" }]);
  // A blank/omitted block decodes to no cells.
  assert.deepEqual(decodeBlock({}), []);
});
