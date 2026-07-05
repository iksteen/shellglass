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
let proto;
let jsTag;
export function setProto(p, js) {
    proto = p;
    jsTag = js;
}
export let reloadPage = () => {
    try {
        const last = Number(sessionStorage.getItem("sg-reload") ?? 0);
        if (Date.now() - last < 5000)
            return;
        sessionStorage.setItem("sg-reload", String(Date.now()));
    }
    catch (e) {
    }
    location.reload();
};
export function setReloadPage(f) {
    reloadPage = f;
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
let cellW = 8;
let cellH = 17;
let dpr = 1;
function measureMetrics() {
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
const ARMS = "0101020210102020" +
    "0000000000000000" +
    "0000000000000000" +
    "0110021001200220" +
    "0011001200210022" +
    "1100120021002200" +
    "1001100220012002" +
    "1110121021101120" +
    "2120221012202220" +
    "1011101220111021" +
    "2021201210222022" +
    "0111011202110212" +
    "0121012202210222" +
    "1101110212011202" +
    "2101210222012202" +
    "1111111212111212" +
    "2111112121212112" +
    "2211112212212212" +
    "1222212222212222" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0000000000000000" +
    "0001100001000010" +
    "0002200002000020" +
    "0201102001022010";
function boxArms(cp) {
    if (cp < 0x2500 || cp > 0x257f)
        return null;
    const o = (cp - 0x2500) * 4;
    const u = +ARMS[o];
    const r = +ARMS[o + 1];
    const d = +ARMS[o + 2];
    const l = +ARMS[o + 3];
    return u || r || d || l ? [u, r, d, l] : null;
}
export function isCanvasGlyph(cp) {
    return cp >= 0x2500 && cp <= 0x259f;
}
function cellFg(cell, isCursor) {
    let fg = resolveRgb(cell.f) ?? parseHex(cfg.defFg);
    if (!!cell.n !== isCursor)
        fg = resolveRgb(cell.g) ?? parseHex(cfg.defBg);
    if (cell.d)
        fg = [Math.floor(fg[0] / 10) * 6, Math.floor(fg[1] / 10) * 6, Math.floor(fg[2] / 10) * 6];
    return fg;
}
let canvasEl = null;
let ctx = null;
function attachCanvas(cols, rows, screenDiv) {
    const c = document.createElement("canvas");
    c.width = Math.round(cols * cellW * dpr);
    c.height = Math.round(rows * cellH * dpr);
    c.style.cssText =
        `position:absolute;top:0;left:0;width:${cols * cellW}px;height:${rows * cellH}px;pointer-events:none`;
    screenDiv.appendChild(c);
    canvasEl = c;
    ctx = c.getContext("2d");
}
function lineWidth(weight) {
    const light = Math.max(1, Math.round(dpr));
    return weight === 2 ? 2 * light : light;
}
function cellRect(r, c) {
    return [
        Math.round(c * cellW * dpr),
        Math.round(r * cellH * dpr),
        Math.round((c + 1) * cellW * dpr),
        Math.round((r + 1) * cellH * dpr),
    ];
}
function drawArms(x0, y0, x1, y1, arms) {
    const [u, r, d, l] = arms;
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    const vw = lineWidth(Math.max(u, d));
    const hw = lineWidth(Math.max(l, r));
    const hvw = vw >> 1;
    const hhw = hw >> 1;
    if (u || d)
        ctx.fillRect(midX - hvw, u ? y0 : midY - hhw, vw, (d ? y1 : midY + hhw) - (u ? y0 : midY - hhw));
    if (l || r)
        ctx.fillRect(l ? x0 : midX - hvw, midY - hhw, (r ? x1 : midX + hvw) - (l ? x0 : midX - hvw), hw);
}
function drawDashes(x0, y0, x1, y1, cp) {
    let horiz, n, weight;
    if (cp <= 0x250b) {
        const k = cp - 0x2504;
        horiz = (k & 3) < 2;
        n = k < 4 ? 3 : 4;
        weight = k & 1 ? 2 : 1;
    }
    else {
        const k = cp - 0x254c;
        horiz = k < 2;
        n = 2;
        weight = k & 1 ? 2 : 1;
    }
    const t = lineWidth(weight);
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    for (let i = 0; i < n; i++) {
        const s0 = (i + 0.2) / n;
        const s1 = (i + 0.8) / n;
        if (horiz) {
            const a = Math.round(x0 + s0 * (x1 - x0));
            const b = Math.round(x0 + s1 * (x1 - x0));
            ctx.fillRect(a, midY - (t >> 1), b - a, t);
        }
        else {
            const a = Math.round(y0 + s0 * (y1 - y0));
            const b = Math.round(y0 + s1 * (y1 - y0));
            ctx.fillRect(midX - (t >> 1), a, t, b - a);
        }
    }
}
const DOUBLES = [
    [0, 0, 1, 1, 0, 1], [1, 1, 0, 0, 1, 0], [0, 1, 0, 1, 0, 1], [0, 1, 0, 1, 1, 0],
    [0, 1, 0, 1, 1, 1], [0, 1, 1, 0, 0, 1], [0, 1, 1, 0, 1, 0], [0, 1, 1, 0, 1, 1],
    [1, 0, 0, 1, 0, 1], [1, 0, 0, 1, 1, 0], [1, 0, 0, 1, 1, 1], [1, 0, 1, 0, 0, 1],
    [1, 0, 1, 0, 1, 0], [1, 0, 1, 0, 1, 1], [1, 1, 0, 1, 0, 1], [1, 1, 0, 1, 1, 0],
    [1, 1, 0, 1, 1, 1], [1, 1, 1, 0, 0, 1], [1, 1, 1, 0, 1, 0], [1, 1, 1, 0, 1, 1],
    [0, 1, 1, 1, 0, 1], [0, 1, 1, 1, 1, 0], [0, 1, 1, 1, 1, 1], [1, 0, 1, 1, 0, 1],
    [1, 0, 1, 1, 1, 0], [1, 0, 1, 1, 1, 1], [1, 1, 1, 1, 0, 1], [1, 1, 1, 1, 1, 0],
    [1, 1, 1, 1, 1, 1],
];
function drawDoubles(x0, y0, x1, y1, cp) {
    const [u, d, l, r, vd, hd] = DOUBLES[cp - 0x2550];
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    const t = lineWidth(1);
    const off = t;
    const maxDv = hd ? off : 0;
    const maxDh = vd ? off : 0;
    const h = t >> 1;
    if (u || d) {
        for (const xc of vd ? [midX - off, midX + off] : [midX]) {
            const a = u ? y0 : midY - maxDv - h;
            const b = d ? y1 : midY + maxDv + h;
            ctx.fillRect(Math.round(xc) - h, a, t, b - a);
        }
    }
    if (l || r) {
        for (const yc of hd ? [midY - off, midY + off] : [midY]) {
            const a = l ? x0 : midX - maxDh - h;
            const b = r ? x1 : midX + maxDh + h;
            ctx.fillRect(a, Math.round(yc) - h, b - a, t);
        }
    }
}
function drawArc(x0, y0, x1, y1, cp) {
    const corners = [[x1, y1], [x0, y1], [x0, y0], [x1, y0]];
    const angles = [
        [Math.PI, 1.5 * Math.PI], [1.5 * Math.PI, 2 * Math.PI],
        [0, 0.5 * Math.PI], [0.5 * Math.PI, Math.PI],
    ];
    const [cx, cy] = corners[cp - 0x256d];
    const [a0, a1] = angles[cp - 0x256d];
    ctx.strokeStyle = ctx.fillStyle;
    ctx.lineWidth = lineWidth(1);
    ctx.beginPath();
    ctx.ellipse(cx, cy, (x1 - x0) / 2, (y1 - y0) / 2, 0, a0, a1);
    ctx.stroke();
}
function drawDiag(x0, y0, x1, y1, cp) {
    ctx.strokeStyle = ctx.fillStyle;
    ctx.lineWidth = lineWidth(1);
    ctx.beginPath();
    if (cp !== 0x2572) {
        ctx.moveTo(x0, y1);
        ctx.lineTo(x1, y0);
    }
    if (cp !== 0x2571) {
        ctx.moveTo(x0, y0);
        ctx.lineTo(x1, y1);
    }
    ctx.stroke();
}
const QUADRANTS = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
function drawBlock(x0, y0, x1, y1, cp) {
    const W = x1 - x0;
    const H = y1 - y0;
    const R = (u0, v0, u1, v1) => {
        const a = Math.round(x0 + u0 * W), b = Math.round(x0 + u1 * W);
        const c = Math.round(y0 + v0 * H), d = Math.round(y0 + v1 * H);
        ctx.fillRect(a, c, b - a, d - c);
    };
    if (cp === 0x2580)
        return R(0, 0, 1, 0.5);
    if (cp >= 0x2581 && cp <= 0x2588)
        return R(0, 1 - (cp - 0x2580) / 8, 1, 1);
    if (cp >= 0x2589 && cp <= 0x258f)
        return R(0, 0, (0x2590 - cp) / 8, 1);
    if (cp === 0x2590)
        return R(0.5, 0, 1, 1);
    if (cp <= 0x2593) {
        const g = ctx.globalAlpha;
        ctx.globalAlpha = (cp - 0x2590) / 4;
        R(0, 0, 1, 1);
        ctx.globalAlpha = g;
        return;
    }
    if (cp === 0x2594)
        return R(0, 0, 1, 0.125);
    if (cp === 0x2595)
        return R(0.875, 0, 1, 1);
    const m = QUADRANTS[cp - 0x2596];
    if (m & 1)
        R(0, 0, 0.5, 0.5);
    if (m & 2)
        R(0.5, 0, 1, 0.5);
    if (m & 4)
        R(0, 0.5, 0.5, 1);
    if (m & 8)
        R(0.5, 0.5, 1, 1);
}
function drawGlyph(r, c, cp, cell, isCursor) {
    if (!ctx)
        return;
    ctx.fillStyle = hex(cellFg(cell, isCursor));
    const [x0, y0, x1, y1] = cellRect(r, c);
    const arms = boxArms(cp);
    if (arms)
        drawArms(x0, y0, x1, y1, arms);
    else if ((cp >= 0x2504 && cp <= 0x250b) || (cp >= 0x254c && cp <= 0x254f))
        drawDashes(x0, y0, x1, y1, cp);
    else if (cp >= 0x2550 && cp <= 0x256c)
        drawDoubles(x0, y0, x1, y1, cp);
    else if (cp >= 0x256d && cp <= 0x2570)
        drawArc(x0, y0, x1, y1, cp);
    else if (cp >= 0x2571 && cp <= 0x2573)
        drawDiag(x0, y0, x1, y1, cp);
    else if (cp >= 0x2580 && cp <= 0x259f)
        drawBlock(x0, y0, x1, y1, cp);
}
function redrawCanvasRow(r) {
    if (!ctx || !canvasEl)
        return;
    const row = screen.cells[r];
    const y0 = Math.round(r * cellH * dpr);
    const y1 = Math.round((r + 1) * cellH * dpr);
    ctx.clearRect(0, y0, canvasEl.width, y1 - y0);
    if (!row)
        return;
    let c = 0;
    for (const cell of row) {
        const w = cell.w ? 2 : 1;
        const cp = cell.t ? cell.t.codePointAt(0) : 0;
        if (cp && isCanvasGlyph(cp)) {
            const isCursor = !!screen.cur && screen.cur[0] === r && screen.cur[1] === c;
            drawGlyph(r, c, cp, cell, isCursor);
        }
        c += w;
    }
}
function redrawCanvasAll() {
    if (!ctx || !canvasEl)
        return;
    ctx.clearRect(0, 0, canvasEl.width, canvasEl.height);
    for (let r = 0; r < screen.cells.length; r++)
        redrawCanvasRow(r);
}
export function isFillGlyph(cp) {
    return ((cp >= 0xe0b0 && cp <= 0xe0d4) ||
        (cp >= 0x2500 && cp <= 0x259f) ||
        (cp >= 0x1fb00 && cp <= 0x1fbaf));
}
export function isMergeableFill(cp) {
    return (cp === 0x2500 ||
        cp === 0x2501 ||
        cp === 0x2550 ||
        cp === 0x2588 ||
        (cp >= 0x2581 && cp <= 0x2587) ||
        cp === 0x2594);
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
function symbolSpan(col, w, boxStyle, font, glyph, first) {
    const fill = isFillGlyph(first);
    const par = fill ? "none" : "xMidYMid meet";
    const stretch = fill ? ' textLength="14" lengthAdjust="spacingAndGlyphs"' : "";
    return (`<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
        `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
        `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor"${stretch}>${glyph}</text></svg></span>`);
}
function symbolCell(cell, isCursor, col, w, font) {
    const boxStyle = cellStyle(cell, isCursor);
    const t = cell.t ?? " ";
    return symbolSpan(col, w, boxStyle, font, esc(t), t.codePointAt(0) ?? 0x20);
}
export function renderRow(cells, cursorCol) {
    let out = "";
    let col = 0;
    let runStyle = null;
    let runCol = 0;
    let cols = 0;
    let text = "";
    const flushText = () => {
        if (text.length === 0)
            return;
        out += `<span class="run" style="left:${runCol}ch;width:${cols}ch;${runStyle ?? ""}">${text}</span>`;
        text = "";
    };
    let fill = null;
    const flushFill = () => {
        if (fill) {
            out += symbolSpan(fill.col, fill.width, fill.style, fill.font, fill.glyph, fill.first);
            fill = null;
        }
    };
    for (const cell of cells) {
        const isCursor = col === cursorCol;
        const w = cell.w ? 2 : 1;
        const cp0 = cell.t ? cell.t.codePointAt(0) : 0;
        if (cp0 && isCanvasGlyph(cp0)) {
            flushText();
            flushFill();
            runStyle = null;
            cols = 0;
            out += `<span class="run" style="left:${col}ch;width:${w}ch;${cellStyle(cell, isCursor)}color:transparent">${esc(cell.t)}</span>`;
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
                }
                else {
                    flushFill();
                    fill = { col, width: w, t, glyph: esc(t), style, font, first };
                }
            }
            else {
                flushFill();
                out += symbolCell(cell, isCursor, col, w, font);
            }
        }
        else {
            flushFill();
            const style = cellStyle(cell, isCursor);
            if (runStyle !== style) {
                flushText();
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
    flushText();
    flushFill();
    return out;
}
function cursorCol(cur, row) {
    return cur && cur[0] === row ? cur[1] : -1;
}
let screen = { cells: [], cur: null, rowEls: [] };
let screenEl;
export function patchCells(state, dp) {
    const dirty = new Set();
    if (dp.cur !== undefined) {
        if (state.cur)
            dirty.add(state.cur[0]);
        if (dp.cur)
            dirty.add(dp.cur[0]);
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
            while (row.length < i)
                row.push({ t: " " });
            row[i] = patch.cells[dx];
        }
        dirty.add(patch.r);
    }
    return dirty;
}
function applyFull(m) {
    const cur = m.p ?? null;
    const rows = m.d.map(decodeBlock);
    let html = `<div class="screen" style="width:${m.w}ch;height:calc(${m.h} * var(--lh));">`;
    for (let r = 0; r < rows.length; r++) {
        html += `<div class="row">${renderRow(rows[r], cursorCol(cur, r))}</div>`;
    }
    html += "</div>";
    screenEl.innerHTML = html;
    const screenDiv = screenEl.firstElementChild;
    screen = {
        cells: rows,
        cur,
        rowEls: Array.from(screenDiv.children),
    };
    attachCanvas(m.w, m.h, screenDiv);
    redrawCanvasAll();
}
function decodeRow([r, l, text, style]) {
    if (typeof text === "string") {
        const st = style;
        const cells = [];
        for (const ch of text)
            cells.push(st ? { t: ch, ...st } : { t: ch });
        return { r, l, cells };
    }
    return { r, l, cells: decodeCells(text, style) };
}
function applyPatches(cur, rows) {
    const dirty = patchCells(screen, { cur, rows });
    for (const r of dirty) {
        const el = screen.rowEls[r];
        if (!el)
            continue;
        el.innerHTML = renderRow(screen.cells[r] ?? [], cursorCol(screen.cur, r));
        redrawCanvasRow(r);
    }
}
function applyDiff(m) {
    applyPatches(m.p, (m.r ?? []).map(decodeRow));
}
function applyCell(m) {
    const { c: r, p: _p, ...style } = m;
    const styled = Object.keys(style).length > 0;
    const cells = [];
    for (const ch of r[2])
        cells.push(styled ? { t: ch, ...style } : { t: ch });
    applyPatches(m.p, [{ r: r[0], l: r[1], cells }]);
}
function applyLine(m) {
    applyPatches(m.p, [decodeRow(m.l)]);
}
function applyBanner(m) {
    screenEl.innerHTML = m.b;
    screen = { cells: [], cur: null, rowEls: [] };
}
export function apply(m) {
    if ("v" in m) {
        const wireChanged = proto !== undefined && m.v !== proto;
        const jsChanged = jsTag !== undefined && m.js !== undefined && m.js !== jsTag;
        if (wireChanged || jsChanged)
            reloadPage();
        return;
    }
    if ("c" in m)
        applyCell(m);
    else if ("l" in m)
        applyLine(m);
    else if ("d" in m)
        applyFull(m);
    else if ("b" in m)
        applyBanner(m);
    else
        applyDiff(m);
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
    setProto(boot.proto, boot.js);
    screenEl = document.getElementById("screen");
    measureMetrics();
    document.fonts?.ready.then(() => {
        measureMetrics();
        redrawCanvasAll();
    });
    connect(boot.events);
}
if (typeof document !== "undefined" && window.SHELLGLASS) {
    main();
}
