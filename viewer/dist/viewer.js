export function decodeCells(text, runs) {
    const cells = [];
    for (const v of text) {
        if (typeof v === "number")
            cells.push({ t: "" });
        else if (typeof v === "string")
            for (const ch of v)
                cells.push({ t: ch });
        else
            cells.push({ t: v[0] });
    }
    for (const [start, len, st] of runs ?? []) {
        for (let i = start; i < start + len && i < cells.length; i++) {
            cells[i] = { t: cells[i].t, ...st };
        }
    }
    return cells;
}
export function decodeBlock(block) {
    return decodeCells(block[0] ?? [], block[1]);
}
let cfg;
export function setConfig(c) {
    cfg = c;
}
const BASE16 = [
    [0x00, 0x00, 0x00], [0xcd, 0x00, 0x00], [0x00, 0xcd, 0x00], [0xcd, 0xcd, 0x00],
    [0x00, 0x00, 0xee], [0xcd, 0x00, 0xcd], [0x00, 0xcd, 0xcd], [0xe5, 0xe5, 0xe5],
    [0x7f, 0x7f, 0x7f], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
    [0x5c, 0x5c, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
];
export function palette(i) {
    if (i < 16)
        return BASE16[i];
    if (i < 232) {
        const n = i - 16;
        const L = [0, 95, 135, 175, 215, 255];
        return [L[Math.floor(n / 36)], L[Math.floor(n / 6) % 6], L[n % 6]];
    }
    const v = 8 + 10 * (i - 232);
    return [v, v, v];
}
function hex(c) {
    return "#" + c.map((x) => x.toString(16).padStart(2, "0")).join("");
}
function parseHex(s) {
    return [
        parseInt(s.slice(1, 3), 16),
        parseInt(s.slice(3, 5), 16),
        parseInt(s.slice(5, 7), 16),
    ];
}
export function resolveRgb(c) {
    if (c == null)
        return null;
    if (typeof c === "number")
        return palette(c);
    return c;
}
export function cellStyle(cell, isCursor) {
    let fg = resolveRgb(cell.f);
    let bg = resolveRgb(cell.g);
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
    if (fg)
        s += `color:${hex(fg)};`;
    if (bg)
        s += `background:${hex(bg)};`;
    if (cell.b)
        s += "font-weight:bold;";
    if (cell.i)
        s += "font-style:italic;";
    if (cell.u)
        s += "text-decoration:underline;";
    return s;
}
export function isFillGlyph(cp) {
    return ((cp >= 0xe0b0 && cp <= 0xe0d4) ||
        (cp >= 0x2500 && cp <= 0x259f) ||
        (cp >= 0x1fb00 && cp <= 0x1fbaf));
}
function symbolFamily(cp) {
    for (const [lo, hi, fam] of cfg.sym) {
        if (cp >= lo && cp <= hi)
            return fam;
    }
    return null;
}
function svgFont(cell) {
    const t = cell.t ?? "";
    if (!t)
        return null;
    const cp = t.codePointAt(0);
    const fam = symbolFamily(cp);
    if (fam)
        return fam;
    return isFillGlyph(cp) ? cfg.fillFont : null;
}
function esc(s) {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
function symbolCell(cell, isCursor, col, w, font) {
    const boxStyle = cellStyle(cell, isCursor);
    const t = cell.t ?? " ";
    const first = t.codePointAt(0) ?? 0x20;
    const par = isFillGlyph(first) ? "none" : "xMidYMid meet";
    return (`<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
        `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
        `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor">${esc(t)}</text></svg></span>`);
}
export function renderRow(cells, cursorCol) {
    let out = "";
    let col = 0;
    let runStyle = null;
    let runCol = 0;
    let cols = 0;
    let text = "";
    const flush = () => {
        if (text.length === 0)
            return;
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
        }
        else {
            const style = cellStyle(cell, isCursor);
            if (runStyle !== style) {
                flush();
                runStyle = style;
                cols = 0;
            }
            if (cols === 0)
                runCol = col;
            text += esc(cell.t && cell.t.length ? cell.t : " ");
            cols += w;
        }
        col += w;
    }
    flush();
    return out;
}
function cursorCol(cur, row) {
    return cur && cur[0] === row ? cur[1] : -1;
}
let screen = { cells: [], cur: null, rowEls: [] };
let screenEl;
export function patchCells(state, dp) {
    const dirty = new Set();
    if (state.cur)
        dirty.add(state.cur[0]);
    if (dp.cur)
        dirty.add(dp.cur[0]);
    state.cur = dp.cur;
    for (const patch of dp.rows) {
        let row = state.cells[patch.r];
        if (!row) {
            row = [];
            state.cells[patch.r] = row;
        }
        for (let dx = 0; dx < patch.cells.length; dx++) {
            const i = patch.l + dx;
            while (row.length < i)
                row.push({ t: " " });
            row[i] = patch.cells[dx];
        }
        dirty.add(patch.r);
    }
    return dirty;
}
function applyFull(m) {
    const rows = m.rows.map(decodeBlock);
    let html = `<div class="screen" style="width:${m.w}ch;height:calc(${m.h} * var(--lh));">`;
    for (let r = 0; r < rows.length; r++) {
        html += `<div class="row">${renderRow(rows[r], cursorCol(m.cur, r))}</div>`;
    }
    html += "</div>";
    screenEl.innerHTML = html;
    const screenDiv = screenEl.firstElementChild;
    screen = {
        cells: rows,
        cur: m.cur,
        rowEls: Array.from(screenDiv.children),
    };
}
function applyDiff(m) {
    const rows = m.rows.map(([r, l, text, runs]) => ({
        r,
        l,
        cells: decodeCells(text, runs),
    }));
    const dirty = patchCells(screen, { cur: m.cur, rows });
    for (const r of dirty) {
        const el = screen.rowEls[r];
        if (!el)
            continue;
        el.innerHTML = renderRow(screen.cells[r] ?? [], cursorCol(screen.cur, r));
    }
}
function applyBanner(m) {
    screenEl.innerHTML = m.html;
    screen = { cells: [], cur: null, rowEls: [] };
}
export function apply(m) {
    if (m.t === "f")
        applyFull(m);
    else if (m.t === "d")
        applyDiff(m);
    else
        applyBanner(m);
}
function connect(events) {
    const es = new EventSource(events);
    es.onmessage = (e) => apply(JSON.parse(e.data));
    es.onerror = () => {
        if (es.readyState === EventSource.CLOSED) {
            setTimeout(() => connect(events), 2000);
        }
    };
}
function main() {
    const boot = window.SHELLGLASS;
    setConfig(boot.cfg);
    screenEl = document.getElementById("screen");
    connect(boot.events);
}
if (typeof document !== "undefined" && window.SHELLGLASS) {
    main();
}
