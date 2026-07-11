// Unit tests for the browser renderer's pure logic (no DOM needed). Run with
// `node --test` — Node strips the TypeScript types on import.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  palette,
  resolveRgb,
  cellStyle,
  applyDefaults,
  linkHref,
  ghostText,
  ghostSpan,
  isFillGlyph,
  isCanvasGlyph,
  glyphOps,
  sextantMask,
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

test("applyDefaults overrides the config defaults and reverts", () => {
  // Override the bg only: inverse on a default cell now swaps in the new bg.
  const css = applyDefaults([null, [0x30, 0x0a, 0x24]]);
  assert.deepEqual(css, { fg: "", bg: "#300a24" });
  assert.equal(cellStyle({ n: true }, false), "color:#300a24;background:#d0d0d0;");
  // An absent `e` (the next full frame without overrides) reverts everything.
  assert.deepEqual(applyDefaults(undefined), { fg: "", bg: "" });
  assert.equal(cellStyle({ n: true }, false), "color:#000000;background:#d0d0d0;");
});

test("cellStyle renders modern SGR: underline styles, strike, underline color", () => {
  // u carries the style number: 1 plain, 3 wavy undercurl, 2/4/5 the rest.
  assert.equal(cellStyle({ u: 1 }, false), "text-decoration:underline;");
  assert.equal(cellStyle({ u: 3 }, false), "text-decoration:underline wavy;");
  assert.equal(cellStyle({ u: 2 }, false), "text-decoration:underline double;");
  assert.equal(cellStyle({ u: 4 }, false), "text-decoration:underline dotted;");
  assert.equal(cellStyle({ u: 5 }, false), "text-decoration:underline dashed;");
  // Strikethrough alone, and combined with an underline.
  assert.equal(cellStyle({ s: 1 }, false), "text-decoration:line-through;");
  assert.equal(
    cellStyle({ u: 1, s: 1 }, false),
    "text-decoration:underline line-through;",
  );
  // Underline color rides the shorthand; absent = currentcolor.
  assert.equal(
    cellStyle({ u: 3, k: 9 }, false),
    "text-decoration:underline wavy #ff0000;",
  );
  assert.equal(
    cellStyle({ u: 1, k: [1, 2, 3] }, false),
    "text-decoration:underline #010203;",
  );
});

test("renderRow coalesces same-style cells into one positioned run", () => {
  const html = renderRow([{ t: "a" }, { t: "b" }, { t: "c" }], -1);
  assert.equal(html, '<span class="run" style="left:0ch;width:3ch;">abc</span>');
});

test("renderRow turns OSC 8 runs into safe anchors", () => {
  const links = { 1: "https://example.com/?q=\"x\"", 2: "javascript:alert(1)" };
  // Linked cells coalesce into one anchor; href is attribute-escaped; the
  // neighbours stay plain spans.
  const html = renderRow(
    [{ t: "x" }, { t: "a", a: 1 }, { t: "b", a: 1 }, { t: "y" }],
    -1,
    0,
    links,
  );
  assert.equal(
    html,
    '<span class="run" style="left:0ch;width:1ch;">x</span>' +
      '<a class="run" href="https://example.com/?q=&quot;x&quot;" target="_blank" rel="noopener noreferrer" style="left:1ch;width:2ch;">ab</a>' +
      '<span class="run" style="left:3ch;width:1ch;">y</span>',
  );
  // Adjacent cells with different links must not merge into one anchor.
  const two = renderRow([{ t: "a", a: 1 }, { t: "b", a: 2 }], -1, 0, {
    1: "https://a.example",
    2: "https://b.example",
  });
  assert.equal(two.match(/<a /g)?.length, 2);
  // A blank cell must not ride a linked run (no clickable gaps).
  const gap = renderRow([{ t: "a", a: 1 }, {}, { t: "b", a: 1 }], -1, 0, {
    1: "https://a.example",
  });
  assert.equal(gap.match(/<a /g)?.length, 2, "blank splits the anchor");
});

test("linkHref allowlists schemes and tolerates pruned ids", () => {
  const links = { 1: "https://ok", 2: "javascript:alert(1)", 3: "DATA:text/html,x", 4: "MAILTO:a@b" };
  assert.equal(linkHref(links, 1), "https://ok");
  assert.equal(linkHref(links, 2), null, "javascript: refused");
  assert.equal(linkHref(links, 3), null, "data: refused");
  assert.equal(linkHref(links, 4), "MAILTO:a@b", "scheme match is case-insensitive");
  assert.equal(
    linkHref({ 5: "file://host/home/x" }, 5),
    "file://host/home/x",
    "ls --hyperlink emits file:",
  );
  assert.equal(linkHref(links, 9), null, "pruned id renders unlinked");
  assert.equal(linkHref(links, undefined), null);
});

test("renderRow draws DECSCUSR cursor shapes", () => {
  const cells = [{ t: "a" }, { t: "b" }];
  // Default/block styles (0-2): the classic reverse-video cell.
  assert.equal(
    renderRow(cells, 0),
    '<span class="run" style="left:0ch;width:1ch;color:#000000;background:#d0d0d0;">a</span>' +
      '<span class="run" style="left:1ch;width:1ch;">b</span>',
  );
  assert.equal(renderRow(cells, 0, 2), renderRow(cells, 0));
  // Bar (5/6): no reverse video, an inset left-edge decoration instead.
  assert.equal(
    renderRow(cells, 0, 5),
    '<span class="run" style="left:0ch;width:1ch;box-shadow:inset 0.14em 0 0 0 currentColor;">a</span>' +
      '<span class="run" style="left:1ch;width:1ch;">b</span>',
  );
  // Underline (3/4): bottom-edge decoration.
  assert.ok(renderRow(cells, 0, 4).includes("inset 0 -0.14em"));
  // A blank cursor cell must not coalesce its decoration away into the run.
  const blanks = [{ t: "x" }, {}, { t: "y" }];
  assert.ok(
    renderRow(blanks, 1, 6).includes("box-shadow"),
    "bar cursor visible on a blank cell",
  );
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

test("box/block/legacy/powerline glyphs route to the canvas as transparent text", () => {
  // These ranges draw on the overlay canvas; the DOM keeps the real glyph as
  // transparent text (no SVG) so it stays selectable/copyable.
  const ranges = [[0x2500, 0x259f], [0x1fb00, 0x1fb3b], [0x1fb70, 0x1fb7b], [0xe0b0, 0xe0b3]];
  for (const [lo, hi] of ranges) {
    for (let cp = lo; cp <= hi; cp++) {
      assert.ok(isCanvasGlyph(cp), `U+${cp.toString(16)} should be canvas-routed`);
    }
  }
  for (const cp of [0x2502, 0x253c, 0x2550, 0x256d, 0x2591, 0x2588, 0x259a, 0x1fb00, 0x1fb70, 0xe0b0]) {
    const g = String.fromCodePoint(cp);
    const html = renderRow([{ t: g }], -1);
    assert.doesNotMatch(html, /<svg/, `U+${cp.toString(16)} emitted SVG`);
    assert.match(html, new RegExp(`color:transparent">${g}</span>`), `U+${cp.toString(16)} not transparent`);
  }
});

test("isCanvasGlyph and glyphOps stay in lockstep (no invisible routing)", () => {
  // A codepoint routed to the canvas but yielding no ops would paint nothing under its
  // transparent DOM text. Sweep the neighbourhoods and assert the two agree.
  for (let cp = 0x2500; cp <= 0x1fbff; cp++) {
    if (cp === 0x25a0) cp = 0xe0a0; // skip the gap between box-drawing and powerline
    if (cp === 0xe100) cp = 0x1fb00; // …and between powerline and legacy-computing
    const has = glyphOps(cp, 0, 0, 10, 20, 1).length > 0;
    assert.equal(has, isCanvasGlyph(cp), `U+${cp.toString(16)}: ops=${has} canvas=${isCanvasGlyph(cp)}`);
  }
});

test("non-canvas fill glyphs (wedges/flames) still take the SVG path", () => {
  // The seam-motivated subset moved to the canvas; the long tail (smooth-mosaic wedges,
  // rounded/flame separators) stays on the stretched-SVG font path.
  for (const cp of [0xe0b8, 0x1fb3c, 0x1fb8c]) {
    assert.ok(!isCanvasGlyph(cp));
    assert.ok(isFillGlyph(cp));
    assert.match(renderRow([{ t: String.fromCodePoint(cp) }], -1), /<svg /);
  }
});

test("symbol_map overrides the canvas for PUA arrows but not standard box glyphs", () => {
  // A user who maps the powerline arrows to a Nerd Font wins over the canvas (E0B0–B3);
  // a map over the standard box range loses — the canvas owns it unconditionally.
  setConfig({ ...CFG, sym: [[0x2500, 0x259f, "Box Font"], [0xe0b0, 0xe0b3, "Arrow Font"]] });
  const arrow = renderRow([{ t: String.fromCodePoint(0xe0b0) }], -1);
  assert.match(arrow, /font-family="Arrow Font"/, "mapped PUA arrow should take the SVG font path");
  assert.doesNotMatch(arrow, /color:transparent/, "mapped arrow should not stay a canvas glyph");
  const box = renderRow([{ t: String.fromCodePoint(0x2500) }], -1);
  assert.doesNotMatch(box, /<svg/, "canvas box glyph must ignore its symbol_map entry");
  assert.match(box, /color:transparent/);
  setConfig(CFG);
});

test("glyphOps emits pure device-pixel ops for the box/block range", () => {
  // Cell rect 0,0..10,20 with light=1 → midX 5, midY 10.
  const ops = (cp: number) => glyphOps(cp, 0, 0, 10, 20, 1);

  // ─ light horizontal: two rects meeting at centre, 1px tall.
  assert.deepEqual(ops(0x2500), [
    { t: "rect", x: 0, y: 10, w: 5, h: 1 },
    { t: "rect", x: 5, y: 10, w: 5, h: 1 },
  ]);
  // ━ heavy horizontal: 2px tall.
  assert.equal(ops(0x2501).every((o) => o.t === "rect" && o.h === 2), true);
  // │ light vertical: two 1px-wide rects centred at x=5.
  assert.deepEqual(ops(0x2502), [
    { t: "rect", x: 5, y: 0, w: 1, h: 10 },
    { t: "rect", x: 5, y: 10, w: 1, h: 10 },
  ]);

  // ┞ mixes weights: up arm heavy (w=2), down arm light (w=1).
  const t = ops(0x251e).filter((o) => o.t === "rect") as { x: number; y: number; w: number; h: number }[];
  const up = t.find((r) => r.y === 0)!;
  const down = t.find((r) => r.y === 10 && r.h === 10)!;
  assert.equal(up.w, 2, "up arm heavy");
  assert.equal(down.w, 1, "down arm light");

  // ═ double horizontal: two rails at ±off (off = 2·light) about the centre.
  assert.deepEqual(ops(0x2550), [
    { t: "rect", x: 0, y: 8, w: 10, h: 1 },
    { t: "rect", x: 0, y: 12, w: 10, h: 1 },
  ]);
  // ╬ full double cross: 4 rails, centre hole preserved.
  assert.equal(ops(0x256c).length, 4);
  // ╔ double corner: the outer rails reach the outer corner (3,8); the INNER rails
  // stop at the inner corner (7,12) instead of crossing the gap to the outer corner.
  assert.deepEqual(ops(0x2554), [
    { t: "rect", x: 3, y: 8, w: 1, h: 12 }, // outer (left) vertical, top at the outer row
    { t: "rect", x: 7, y: 12, w: 1, h: 8 }, // inner (right) vertical, top at the inner row
    { t: "rect", x: 3, y: 8, w: 7, h: 1 }, // outer (top) horizontal
    { t: "rect", x: 7, y: 12, w: 3, h: 1 }, // inner (bottom) horizontal
  ]);

  // ╭ rounded corner: one elliptical arc, centred on the far corner with radii
  // reaching the arms' centrelines (midpoint + 0.5 for the odd light width).
  assert.deepEqual(ops(0x256d), [
    { t: "arc", cx: 10, cy: 20, rx: 4.5, ry: 9.5, a0: Math.PI, a1: 1.5 * Math.PI, lw: 1 },
  ]);
  // ╱ one diagonal, ╳ two.
  assert.equal(ops(0x2571).length, 1);
  assert.equal(ops(0x2571)[0].t, "line");
  assert.equal(ops(0x2573).length, 2);

  // ░ light shade: one full-cell rect at 0.25 alpha.
  assert.deepEqual(ops(0x2591), [{ t: "rect", x: 0, y: 0, w: 10, h: 20, alpha: 0.25 }]);
  // █ full block: one opaque full-cell rect.
  assert.deepEqual(ops(0x2588), [{ t: "rect", x: 0, y: 0, w: 10, h: 20 }]);
  // ▚ diagonal quadrants: top-left + bottom-right rects.
  assert.deepEqual(ops(0x259a), [
    { t: "rect", x: 0, y: 0, w: 5, h: 10 },
    { t: "rect", x: 5, y: 10, w: 5, h: 10 },
  ]);

  // Every codepoint in the range yields at least one op (no holes).
  for (let cp = 0x2500; cp <= 0x259f; cp++) {
    assert.ok(ops(cp).length > 0, `U+${cp.toString(16)} produced no ops`);
  }
  // Higher DPR scales line thickness: light=2 doubles the ─ rail height.
  assert.equal((glyphOps(0x2500, 0, 0, 10, 20, 2)[0] as { h: number }).h, 2);
});

test("glyphOps emits legacy-computing and powerline geometry", () => {
  // Cell 0,0..16,24 so halves/thirds/eighths land on integers.
  const ops = (cp: number) => glyphOps(cp, 0, 0, 16, 24, 1);

  // Sextant U+1FB00 = top-left cell only (mask 1): one 2×3-grid rect.
  assert.deepEqual(ops(0x1fb00), [{ t: "rect", x: 0, y: 0, w: 8, h: 8 }]);
  // U+1FB3B = every sextant but the top-left (mask 62): five rects.
  assert.equal(ops(0x1fb3b).length, 5);
  // The mask recovery skips the two half-column glyphs (21 ▌, 42 ▐).
  assert.equal(sextantMask(0x1fb00), 1);
  assert.equal(sextantMask(0x1fb3b), 62);

  // Vertical one-eighth bar U+1FB70 (column 2): x in [2,4], full height.
  assert.deepEqual(ops(0x1fb70), [{ t: "rect", x: 2, y: 0, w: 2, h: 24 }]);
  // Horizontal one-eighth bar U+1FB76 (row 2): y in [3,6], full width.
  assert.deepEqual(ops(0x1fb76), [{ t: "rect", x: 0, y: 3, w: 16, h: 3 }]);

  // Powerline E0B0 ►: one solid triangle, apex mid-right; the base bleeds 1px
  // left of the cell edge to close the sub-pixel seam with the abutting segment.
  assert.deepEqual(ops(0xe0b0), [{ t: "poly", pts: [[-1, 0], [16, 12], [-1, 24]] }]);
  // E0B2 ◄: mirror — apex mid-left, base bleeds 1px right.
  assert.deepEqual(ops(0xe0b2), [{ t: "poly", pts: [[17, 0], [0, 12], [17, 24]] }]);
  // E0B1 (hollow): two stroked edges, no fill.
  const hollow = ops(0xe0b1);
  assert.equal(hollow.length, 2);
  assert.ok(hollow.every((o) => o.t === "line"));
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

// ── ghost layer (storm mode's copy/find/a11y surface) ─────────────────────────

test("ghostText: wide cells emit their grapheme once (2ch advance is the font's)", () => {
  assert.equal(ghostText([{ t: "漢", w: 1 }, { t: "x" }] as never), "漢x");
});

test("ghostText: blank and empty cells become spaces, trailing blanks preserved", () => {
  // Trailing blanks stay: the ghost row spans the full grid width so column
  // math (and selections past end-of-text) match the picture on the canvas.
  assert.equal(ghostText([{ t: "a" }, { t: "" }, { t: undefined }] as never), "a  ");
});

test("ghostText: multi-codepoint graphemes survive intact", () => {
  assert.equal(ghostText([{ t: "e\u0301" }, { t: "👩‍🚀", w: 1 }] as never), "e\u0301👩‍🚀");
});

test("cellStyle conceal: glyph transparent, bg stays, decorations keep their ink", () => {
  // conceal hides the glyph but not the cell — bg fills still paint
  assert.equal(cellStyle({ o: 1 }, false), "color:transparent;");
  assert.equal(cellStyle({ o: 1, g: 4 }, false), "color:transparent;background:#0000ee;");
  // transparent currentcolor would erase decorations — the color is pinned
  assert.equal(
    cellStyle({ o: 1, u: 1 }, false),
    "color:transparent;text-decoration:underline #d0d0d0;",
  );
  // an explicit underline color (k) wins as usual
  assert.equal(
    cellStyle({ o: 1, u: 1, k: 9 }, false),
    "color:transparent;text-decoration:underline #ff0000;",
  );
});

test("cellStyle blink animates the ink; conceal wins over blink", () => {
  assert.equal(cellStyle({ x: 1 }, false), "animation:sg-blink 1s step-end infinite;");
  // blinking a concealed cell is meaningless — conceal's transparent wins
  assert.equal(cellStyle({ x: 1, o: 1 }, false), "color:transparent;");
});

// ── ghostSpan (in-place ghost patching) ───────────────────────────────────────

test("ghostSpan finds the minimal splice", () => {
  assert.equal(ghostSpan("same", "same"), null);
  assert.deepEqual(ghostSpan("abcdef", "abcXef"), [3, 1, "X"]);
  assert.deepEqual(ghostSpan("abc", "abXc"), [2, 0, "X"]); // pure insert
  assert.deepEqual(ghostSpan("abXc", "abc"), [2, 1, ""]); // pure delete
  assert.deepEqual(ghostSpan("", "xy"), [0, 0, "xy"]);
  assert.deepEqual(ghostSpan("aaaa", "aaa"), [3, 1, ""]); // repeats: deterministic
  // multi-span changes collapse to one covering splice
  assert.deepEqual(ghostSpan("aXbYc", "aPbQc"), [1, 3, "PbQ"]);
});

test("ghostSpan splices reproduce the target string", () => {
  const cases: [string, string][] = [
    ["left pane   right pane", "left pane   RIGHT PANE"],
    ["𝕒bc", "𝕓bc"], // surrogate pairs at the boundary
    ["prompt $ ", "prompt $ ls"],
  ];
  for (const [old, next] of cases) {
    const s = ghostSpan(old, next);
    assert.ok(s !== null);
    const [a, del, ins] = s;
    assert.equal(old.slice(0, a) + ins + old.slice(a + del), next, `${old} -> ${next}`);
  }
});
