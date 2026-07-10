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
let baseFg = "";
let baseBg = "";
export function setConfig(c) {
    cfg = c;
    baseFg = c.defFg;
    baseBg = c.defBg;
}
export function applyDefaults(e) {
    const fg = resolveRgb(e?.[0]);
    const bg = resolveRgb(e?.[1]);
    cfg.defFg = fg ? hex(fg) : baseFg;
    cfg.defBg = bg ? hex(bg) : baseBg;
    return { fg: fg ? cfg.defFg : "", bg: bg ? cfg.defBg : "" };
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
    if (cell.u || cell.s) {
        let d = `${cell.u ? "underline" : ""}${cell.s ? " line-through" : ""}`;
        const us = { 2: "double", 3: "wavy", 4: "dotted", 5: "dashed" }[cell.u];
        if (us)
            d += ` ${us}`;
        const k = resolveRgb(cell.k);
        if (k)
            d += ` ${hex(k)}`;
        s += `text-decoration:${d.trim()};`;
    }
    return s;
}
let cellW = 8;
let cellH = 17;
let dpr = 1;
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
    return ((cp >= 0x2500 && cp <= 0x259f) ||
        (cp >= 0x1fb00 && cp <= 0x1fb3b) ||
        (cp >= 0x1fb70 && cp <= 0x1fb7b) ||
        (cp >= 0xe0b0 && cp <= 0xe0b3));
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
let fontPx = 16;
let fontFam = "monospace";
const fontMetricsCache = new Map();
function strutMetrics(font) {
    let m = fontMetricsCache.get(font);
    if (!m && ctx) {
        const prev = ctx.font;
        ctx.font = font;
        const tm = ctx.measureText("Mg");
        m = {
            asc: tm.fontBoundingBoxAscent ?? fontPx * 0.8,
            desc: tm.fontBoundingBoxDescent ?? fontPx * 0.25,
        };
        ctx.font = prev;
        fontMetricsCache.set(font, m);
    }
    return m ?? { asc: fontPx * 0.8, desc: fontPx * 0.25 };
}
function rowBaseline(r) {
    const m = strutMetrics(`${fontPx}px ${fontFam}`);
    return Math.round(r * cellH * dpr + (cellH * dpr - (m.asc + m.desc)) / 2 + m.asc);
}
const inkBoxCache = new Map();
function inkBox(font, glyph) {
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
    if (!m || m.l + m.r <= 0 || m.a + m.d <= 0)
        return null;
    return m;
}
let obsScreen = null;
let gCols = 0;
let gRows = 0;
let ro = null;
let dprMedia = null;
function sizeCanvas() {
    fontMetricsCache.clear();
    inkBoxCache.clear();
    if (!canvasEl || !obsScreen || !gCols || !gRows)
        return;
    const rect = obsScreen.getBoundingClientRect();
    if (!rect.width || !rect.height)
        return;
    cellW = rect.width / gCols;
    cellH = rect.height / gRows;
    dpr = window.devicePixelRatio || 1;
    canvasEl.width = Math.round(rect.width * dpr);
    canvasEl.height = Math.round(rect.height * dpr);
    const cs = getComputedStyle(obsScreen);
    const localW = parseFloat(cs.width) || obsScreen.offsetWidth;
    const z = localW > 0 ? rect.width / localW : 1;
    fontPx = parseFloat(cs.fontSize) * z * dpr;
    fontFam = cs.fontFamily;
}
function onDprChange() {
    sizeCanvas();
    redrawCanvasAll();
    watchDpr();
}
function watchDpr() {
    if (typeof matchMedia === "undefined")
        return;
    dprMedia?.removeEventListener("change", onDprChange);
    dprMedia = matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
    dprMedia.addEventListener("change", onDprChange);
}
let zoomHooked = false;
function watchZoom() {
    if (zoomHooked || typeof window === "undefined")
        return;
    zoomHooked = true;
    window.addEventListener("sg-zoom", () => {
        sizeCanvas();
        redrawCanvasAll();
    });
}
function attachCanvas(cols, rows, screenDiv) {
    const c = document.createElement("canvas");
    c.style.cssText = "position:absolute;top:0;left:0;width:100%;height:100%;pointer-events:none";
    screenDiv.appendChild(c);
    canvasEl = c;
    ctx = c.getContext("2d");
    obsScreen = screenDiv;
    gCols = cols;
    gRows = rows;
    sizeCanvas();
    watchZoom();
    if (typeof ResizeObserver !== "undefined") {
        if (!ro)
            ro = new ResizeObserver(() => { sizeCanvas(); redrawCanvasAll(); });
        ro.disconnect();
        ro.observe(screenDiv);
    }
    watchDpr();
}
function cellRect(r, c) {
    return [
        Math.round(c * cellW * dpr),
        Math.round(r * cellH * dpr),
        Math.round((c + 1) * cellW * dpr),
        Math.round((r + 1) * cellH * dpr),
    ];
}
const rectOp = (x, y, w, h, alpha) => alpha === undefined ? { t: "rect", x, y, w, h } : { t: "rect", x, y, w, h, alpha };
function fracRect(x0, y0, x1, y1, u0, v0, u1, v1, alpha) {
    const W = x1 - x0, H = y1 - y0;
    const a = Math.round(x0 + u0 * W), b = Math.round(x0 + u1 * W);
    const c = Math.round(y0 + v0 * H), d = Math.round(y0 + v1 * H);
    return rectOp(a, c, b - a, d - c, alpha);
}
function lw(weight, light) {
    return weight === 2 ? 2 * light : light;
}
function armsOps(x0, y0, x1, y1, arms, light) {
    const [u, r, d, l] = arms;
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    const vh = lw(Math.max(u, d), light) >> 1;
    const hh = lw(Math.max(l, r), light) >> 1;
    const ops = [];
    if (u) {
        const t = lw(u, light);
        ops.push(rectOp(midX - (t >> 1), y0, t, midY + hh - y0));
    }
    if (d) {
        const t = lw(d, light);
        ops.push(rectOp(midX - (t >> 1), midY - hh, t, y1 - (midY - hh)));
    }
    if (l) {
        const t = lw(l, light);
        ops.push(rectOp(x0, midY - (t >> 1), midX + vh - x0, t));
    }
    if (r) {
        const t = lw(r, light);
        ops.push(rectOp(midX - vh, midY - (t >> 1), x1 - (midX - vh), t));
    }
    return ops;
}
function dashesOps(x0, y0, x1, y1, cp, light) {
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
    const t = lw(weight, light);
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    const ops = [];
    for (let i = 0; i < n; i++) {
        const s0 = (i + 0.2) / n;
        const s1 = (i + 0.8) / n;
        if (horiz) {
            const a = Math.round(x0 + s0 * (x1 - x0));
            const b = Math.round(x0 + s1 * (x1 - x0));
            ops.push(rectOp(a, midY - (t >> 1), b - a, t));
        }
        else {
            const a = Math.round(y0 + s0 * (y1 - y0));
            const b = Math.round(y0 + s1 * (y1 - y0));
            ops.push(rectOp(midX - (t >> 1), a, t, b - a));
        }
    }
    return ops;
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
function doublesOps(x0, y0, x1, y1, cp, light) {
    const [u, d, l, r, vd, hd] = DOUBLES[cp - 0x2550];
    const midX = Math.round((x0 + x1) / 2);
    const midY = Math.round((y0 + y1) / 2);
    const t = lw(1, light);
    const h = t >> 1;
    const off = Math.max(1, Math.min(2 * t, Math.floor((x1 - x0) / 2) - h));
    const dbl = vd && hd;
    const oneH = !!l !== !!r;
    const oneV = !!u !== !!d;
    const hDir = r ? 1 : -1;
    const vDir = d ? 1 : -1;
    const ops = [];
    if (u || d) {
        for (const sx of vd ? [-1, 1] : [0]) {
            const xc = midX + sx * off;
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
function arcOps(x0, y0, x1, y1, cp, light) {
    const off = (light % 2) / 2;
    const mx = Math.round((x0 + x1) / 2) + off;
    const my = Math.round((y0 + y1) / 2) + off;
    const corners = [[x1, y1], [x0, y1], [x0, y0], [x1, y0]];
    const angles = [
        [Math.PI, 1.5 * Math.PI], [1.5 * Math.PI, 2 * Math.PI],
        [0, 0.5 * Math.PI], [0.5 * Math.PI, Math.PI],
    ];
    const [cx, cy] = corners[cp - 0x256d];
    const [a0, a1] = angles[cp - 0x256d];
    return [{ t: "arc", cx, cy, rx: Math.abs(cx - mx), ry: Math.abs(cy - my), a0, a1, lw: lw(1, light) }];
}
function diagOps(x0, y0, x1, y1, cp, light) {
    const t = lw(1, light);
    const ops = [];
    if (cp !== 0x2572)
        ops.push({ t: "line", x0, y0: y1, x1, y1: y0, lw: t });
    if (cp !== 0x2571)
        ops.push({ t: "line", x0, y0, x1, y1, lw: t });
    return ops;
}
const QUADRANTS = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
function blockOps(x0, y0, x1, y1, cp) {
    const W = x1 - x0;
    const H = y1 - y0;
    const R = (u0, v0, u1, v1, alpha) => {
        const a = Math.round(x0 + u0 * W), b = Math.round(x0 + u1 * W);
        const c = Math.round(y0 + v0 * H), d = Math.round(y0 + v1 * H);
        return rectOp(a, c, b - a, d - c, alpha);
    };
    if (cp === 0x2580)
        return [R(0, 0, 1, 0.5)];
    if (cp >= 0x2581 && cp <= 0x2588)
        return [R(0, 1 - (cp - 0x2580) / 8, 1, 1)];
    if (cp >= 0x2589 && cp <= 0x258f)
        return [R(0, 0, (0x2590 - cp) / 8, 1)];
    if (cp === 0x2590)
        return [R(0.5, 0, 1, 1)];
    if (cp <= 0x2593)
        return [R(0, 0, 1, 1, (cp - 0x2590) / 4)];
    if (cp === 0x2594)
        return [R(0, 0, 1, 0.125)];
    if (cp === 0x2595)
        return [R(0.875, 0, 1, 1)];
    const m = QUADRANTS[cp - 0x2596];
    const ops = [];
    if (m & 1)
        ops.push(R(0, 0, 0.5, 0.5));
    if (m & 2)
        ops.push(R(0.5, 0, 1, 0.5));
    if (m & 4)
        ops.push(R(0, 0.5, 0.5, 1));
    if (m & 8)
        ops.push(R(0.5, 0.5, 1, 1));
    return ops;
}
export function sextantMask(cp) {
    let m = cp - 0x1fb00 + 1;
    if (m >= 21)
        m += 1;
    if (m >= 42)
        m += 1;
    return m;
}
function sextantOps(x0, y0, x1, y1, cp) {
    const mask = sextantMask(cp);
    const ops = [];
    for (let i = 0; i < 6; i++) {
        if (!(mask & (1 << i)))
            continue;
        const cx = i % 2, cy = (i / 2) | 0;
        ops.push(fracRect(x0, y0, x1, y1, cx / 2, cy / 3, (cx + 1) / 2, (cy + 1) / 3));
    }
    return ops;
}
function eighthBarOps(x0, y0, x1, y1, cp) {
    if (cp <= 0x1fb75) {
        const n = cp - 0x1fb70 + 2;
        return [fracRect(x0, y0, x1, y1, (n - 1) / 8, 0, n / 8, 1)];
    }
    const n = cp - 0x1fb76 + 2;
    return [fracRect(x0, y0, x1, y1, 0, (n - 1) / 8, 1, n / 8)];
}
function powerlineOps(x0, y0, x1, y1, cp, light) {
    const midY = Math.round((y0 + y1) / 2);
    const right = cp === 0xe0b0 || cp === 0xe0b1;
    const ax = right ? x1 : x0;
    const bx = right ? x0 : x1;
    if (cp === 0xe0b0 || cp === 0xe0b2) {
        const bb = bx + (right ? -light : light);
        return [{ t: "poly", pts: [[bb, y0], [ax, midY], [bb, y1]] }];
    }
    const t = lw(1, light);
    return [
        { t: "line", x0: bx, y0, x1: ax, y1: midY, lw: t },
        { t: "line", x0: ax, y0: midY, x1: bx, y1, lw: t },
    ];
}
export function glyphOps(cp, x0, y0, x1, y1, light) {
    const arms = boxArms(cp);
    if (arms)
        return armsOps(x0, y0, x1, y1, arms, light);
    if ((cp >= 0x2504 && cp <= 0x250b) || (cp >= 0x254c && cp <= 0x254f))
        return dashesOps(x0, y0, x1, y1, cp, light);
    if (cp >= 0x2550 && cp <= 0x256c)
        return doublesOps(x0, y0, x1, y1, cp, light);
    if (cp >= 0x256d && cp <= 0x2570)
        return arcOps(x0, y0, x1, y1, cp, light);
    if (cp >= 0x2571 && cp <= 0x2573)
        return diagOps(x0, y0, x1, y1, cp, light);
    if (cp >= 0x2580 && cp <= 0x259f)
        return blockOps(x0, y0, x1, y1, cp);
    if (cp >= 0x1fb00 && cp <= 0x1fb3b)
        return sextantOps(x0, y0, x1, y1, cp);
    if (cp >= 0x1fb70 && cp <= 0x1fb7b)
        return eighthBarOps(x0, y0, x1, y1, cp);
    if (cp >= 0xe0b0 && cp <= 0xe0b3)
        return powerlineOps(x0, y0, x1, y1, cp, light);
    return [];
}
function paintOps(g, color, ops) {
    g.fillStyle = color;
    g.strokeStyle = color;
    for (const op of ops) {
        if (op.t === "rect") {
            if (op.alpha !== undefined) {
                const a = g.globalAlpha;
                g.globalAlpha = op.alpha;
                g.fillRect(op.x, op.y, op.w, op.h);
                g.globalAlpha = a;
            }
            else {
                g.fillRect(op.x, op.y, op.w, op.h);
            }
        }
        else if (op.t === "arc") {
            g.lineWidth = op.lw;
            g.beginPath();
            g.ellipse(op.cx, op.cy, op.rx, op.ry, 0, op.a0, op.a1);
            g.stroke();
        }
        else if (op.t === "poly") {
            g.beginPath();
            g.moveTo(op.pts[0][0], op.pts[0][1]);
            for (let i = 1; i < op.pts.length; i++)
                g.lineTo(op.pts[i][0], op.pts[i][1]);
            g.closePath();
            g.fill();
        }
        else {
            g.lineWidth = op.lw;
            g.beginPath();
            g.moveTo(op.x0, op.y0);
            g.lineTo(op.x1, op.y1);
            g.stroke();
        }
    }
}
function drawGlyph(r, c, cp, cell, isCursor) {
    if (!ctx)
        return;
    const [x0, y0, x1, y1] = cellRect(r, c);
    const light = Math.max(1, Math.round(dpr));
    paintOps(ctx, hex(cellFg(cell, isCursor)), glyphOps(cp, x0, y0, x1, y1, light));
}
function redrawCanvasRow(r) {
    if (!ctx || !canvasEl)
        return;
    if (storm)
        return drawRowStorm(r);
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
            const isCursor = !!screen.cur && screen.cur[0] === r && screen.cur[1] === c && screen.sty <= 2;
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
let storm = false;
let stormHot = 0;
let lastStormy = 0;
let stormTimer = null;
const STORM_RATIO = 0.5;
const STORM_ENTER = 3;
const STORM_EXIT_MS = 1200;
function cellBgRgb(cell, isCursor) {
    if (!!cell.n !== isCursor)
        return resolveRgb(cell.f) ?? parseHex(cfg.defFg);
    return resolveRgb(cell.g);
}
let smoothCursor;
function smoothCursorOn() {
    if (smoothCursor === undefined)
        smoothCursor = new URLSearchParams(location.search).get("cursor") === "smooth";
    return smoothCursor;
}
const CUR_TRAVEL_MS = 80;
let curAnim = null;
let lastCurPos = null;
function startCurAnim(from, to) {
    let fr = from[0];
    let fc = from[1];
    if (curAnim) {
        const k = Math.min(1, (clock() - curAnim.t0) / CUR_TRAVEL_MS);
        const e = 1 - (1 - k) * (1 - k);
        fr = curAnim.fr + (curAnim.tr - curAnim.fr) * e;
        fc = curAnim.fc + (curAnim.tc - curAnim.fc) * e;
    }
    const running = curAnim !== null;
    curAnim = { fr, fc, tr: to[0], tc: to[1], t0: clock(), rows: [] };
    if (!running)
        requestAnimationFrame(stepCurAnim);
}
function stepCurAnim() {
    if (!curAnim)
        return;
    if (!ctx || !storm) {
        curAnim = null;
        return;
    }
    const wipe = curAnim.rows;
    const k = Math.min(1, (clock() - curAnim.t0) / CUR_TRAVEL_MS);
    if (k >= 1) {
        const tr = curAnim.tr;
        curAnim = null;
        for (const r of wipe)
            redrawCanvasRow(r);
        redrawCanvasRow(tr);
        return;
    }
    for (const r of wipe)
        redrawCanvasRow(r);
    const e = 1 - (1 - k) * (1 - k);
    const r = curAnim.fr + (curAnim.tr - curAnim.fr) * e;
    const c = curAnim.fc + (curAnim.tc - curAnim.fc) * e;
    const x0 = Math.round(c * cellW * dpr);
    const x1 = Math.round((c + 1) * cellW * dpr);
    const y0 = Math.round(r * cellH * dpr);
    const y1 = Math.round((r + 1) * cellH * dpr);
    curAnim.rows = [...new Set([Math.floor(r), Math.ceil(r)])].filter((v) => v >= 0 && v < screen.cells.length);
    ctx.fillStyle = cfg.defFg;
    const bar = Math.max(1, Math.round(fontPx * 0.14));
    if (screen.sty >= 5)
        ctx.fillRect(x0, y0, bar, y1 - y0);
    else if (screen.sty >= 3)
        ctx.fillRect(x0, y1 - bar, x1 - x0, bar);
    else
        ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
    requestAnimationFrame(stepCurAnim);
}
let renderBox;
let renderPref;
function canvasModeOn() {
    if (renderBox === undefined) {
        renderBox = document.getElementById("render");
        renderBox?.addEventListener("change", () => {
            if (renderBox?.checked)
                setStorm(true);
            else if (!selectionActive())
                setStorm(false);
        });
    }
    if (renderBox !== null)
        return renderBox.checked;
    if (renderPref === undefined) {
        let stored = null;
        try {
            stored = localStorage.getItem("shellglass-render");
        }
        catch {
        }
        renderPref = stored
            ? stored === "on"
            : new URLSearchParams(location.search).get("render") === "canvas";
    }
    return renderPref;
}
let stormBox;
let stormPref;
function stormAutoOn() {
    if (stormBox === undefined) {
        stormBox = document.getElementById("storm");
        stormBox?.addEventListener("change", () => {
            if (!stormBox?.checked && storm && !canvasModeOn() && !selectionActive())
                setStorm(false);
        });
    }
    if (stormBox !== null)
        return stormBox.checked;
    if (stormPref === undefined) {
        let stored = null;
        try {
            stored = localStorage.getItem("shellglass-storm");
        }
        catch {
        }
        stormPref = stored
            ? stored === "on"
            : new URLSearchParams(location.search).get("storm") !== "off";
    }
    return stormPref;
}
let crtBox;
function crtOn() {
    if (crtBox === undefined) {
        crtBox = document.getElementById("crt");
        crtBox?.addEventListener("change", () => redrawCanvasAll());
    }
    return crtBox !== null && crtBox.checked;
}
function drawRowStorm(r) {
    if (!ctx || !canvasEl)
        return;
    const y0 = Math.round(r * cellH * dpr);
    const y1 = Math.round((r + 1) * cellH * dpr);
    ctx.clearRect(0, y0, canvasEl.width, y1 - y0);
    ctx.save();
    ctx.beginPath();
    ctx.rect(0, y0, canvasEl.width, y1 - y0);
    ctx.clip();
    const imgSpans = [];
    for (const { ref, el } of screenImages) {
        const natW = el.naturalWidth;
        const natH = el.naturalHeight;
        if (!el.complete || !natW || !natH)
            continue;
        const sc = ref.w && ref.h
            ? Math.min((ref.w * cellW * dpr) / natW, (ref.h * cellH * dpr) / natH)
            : dpr;
        const ix = ref.c * cellW * dpr;
        const iy = ref.r * cellH * dpr;
        const top = Math.max(y0, iy);
        const bot = Math.min(y1, iy + natH * sc);
        if (bot <= top)
            continue;
        ctx.drawImage(el, 0, (top - iy) / sc, natW, (bot - top) / sc, ix, top, natW * sc, bot - top);
        imgSpans.push([ix, ix + natW * sc]);
    }
    const row = screen.cells[r];
    if (!row) {
        ctx.restore();
        return;
    }
    ctx.textBaseline = "alphabetic";
    const baseY = rowBaseline(r);
    const defBg = cfg.defBg.toLowerCase();
    const th = Math.max(1, Math.round(fontPx * 0.06));
    const ulY = baseY + Math.max(th, Math.round(fontPx * 0.065));
    const strikeY = baseY - Math.round(fontPx * 0.36);
    const drawUnderline = (x0, x1, style, color) => {
        if (!ctx)
            return;
        ctx.fillStyle = color;
        switch (style) {
            case 2:
                ctx.fillRect(x0, ulY, x1 - x0, th);
                ctx.fillRect(x0, ulY + 2 * th, x1 - x0, th);
                break;
            case 3: {
                const amp = Math.max(1, Math.round(fontPx * 0.045));
                const period = Math.max(6, Math.round(fontPx * 0.5));
                ctx.strokeStyle = color;
                ctx.lineWidth = th;
                ctx.beginPath();
                const step = Math.max(1, Math.round(dpr));
                for (let x = x0; x <= x1; x += step) {
                    const y = ulY + Math.sin((x * 2 * Math.PI) / period) * amp;
                    if (x === x0)
                        ctx.moveTo(x, y);
                    else
                        ctx.lineTo(x, y);
                }
                ctx.stroke();
                break;
            }
            case 4:
                for (let x = x0 - (x0 % (2 * th)); x < x1; x += 2 * th) {
                    if (x >= x0)
                        ctx.fillRect(x, ulY, th, th);
                }
                break;
            case 5:
                for (let x = x0 - (x0 % (5 * th)); x < x1; x += 5 * th) {
                    const lo = Math.max(x, x0);
                    const hi = Math.min(x + 3 * th, x1);
                    if (hi > lo)
                        ctx.fillRect(lo, ulY, hi - lo, th);
                }
                break;
            default:
                ctx.fillRect(x0, ulY, x1 - x0, th);
        }
    };
    const blocky = screen.sty <= 2;
    let curFont = "";
    let c = 0;
    for (const cell of row) {
        const w = cell.w ? 2 : 1;
        const isCursor = curAnim === null && !!screen.cur && screen.cur[0] === r && screen.cur[1] === c;
        const curBlock = isCursor && blocky;
        const x0 = Math.round(c * cellW * dpr);
        const x1 = Math.round((c + w) * cellW * dpr);
        const bg = cellBgRgb(cell, curBlock);
        if (bg && hex(bg) !== defBg) {
            ctx.fillStyle = hex(bg);
            ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
        }
        else if (imgSpans.length &&
            ((cell.t && cell.t !== " ") || bg) &&
            imgSpans.some(([a, b]) => x0 < b && x1 > a)) {
            ctx.fillStyle = defBg;
            ctx.fillRect(x0, y0, x1 - x0, y1 - y0);
        }
        const cp = cell.t ? cell.t.codePointAt(0) : 0;
        if (cp && isCanvasGlyph(cp) && !(cp >= 0xe000 && symbolFamily(cp))) {
            drawGlyph(r, c, cp, cell, curBlock);
        }
        else if (cell.t && cell.t !== " ") {
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
                const sx = (x1 - x0) / (ink.l + ink.r);
                const sy = (y1 - y0) / (ink.a + ink.d);
                ctx.save();
                ctx.translate(x0 + ink.l * sx, y0 + ink.a * sy);
                ctx.scale(sx, sy);
                ctx.fillText(cell.t, 0, 0);
                ctx.restore();
            }
            else if (cp && glyphOverflowsCell(cell.t, w) && !symbolFamily(cp)) {
                ctx.fillText(cell.t, x0, baseY);
            }
            else {
                ctx.fillText(cell.t, x0, baseY, x1 - x0);
            }
            if (cell.u) {
                const ulColor = resolveRgb(cell.k);
                drawUnderline(x0, x1, typeof cell.u === "number" ? cell.u : 1, ulColor ? hex(ulColor) : fg);
                ctx.fillStyle = fg;
            }
            if (cell.s)
                ctx.fillRect(x0, strikeY, x1 - x0, th);
        }
        if (r === hoverRow && cell.a !== undefined && cell.a === hoverA && !cell.u) {
            drawUnderline(x0, x1, 1, hex(cellFg(cell, curBlock)));
        }
        if (isCursor && !blocky) {
            const cw = Math.max(1, Math.round(fontPx * 0.14));
            ctx.fillStyle = hex(cellFg(cell, false));
            if (screen.sty >= 5)
                ctx.fillRect(x0, y0, cw, y1 - y0);
            else
                ctx.fillRect(x0, y1 - cw, x1 - x0, cw);
        }
        c += w;
    }
    if (crtOn()) {
        ctx.save();
        ctx.globalCompositeOperation = "lighter";
        ctx.globalAlpha = 0.4;
        ctx.filter = `blur(${1.5 * dpr}px)`;
        ctx.drawImage(canvasEl, 0, y0, canvasEl.width, y1 - y0, 0, y0, canvasEl.width, y1 - y0);
        ctx.restore();
    }
    ctx.restore();
}
let hoverA;
let hoverRow = -1;
function cellAt(ev) {
    if (!obsScreen || !cellW || !cellH)
        return null;
    const rect = obsScreen.getBoundingClientRect();
    const col = Math.floor((ev.clientX - rect.left) / cellW);
    const r = Math.floor((ev.clientY - rect.top) / cellH);
    const row = screen.cells[r];
    if (!row || col < 0)
        return null;
    let c = 0;
    for (const cell of row) {
        const w = cell.w ? 2 : 1;
        if (col < c + w)
            return { cell, r };
        c += w;
    }
    return null;
}
function setHover(a, r) {
    if (a === hoverA && r === hoverRow)
        return;
    const old = hoverRow;
    hoverA = a;
    hoverRow = r;
    if (obsScreen)
        obsScreen.style.cursor = a === undefined ? "" : "pointer";
    if (old >= 0)
        redrawCanvasRow(old);
    if (r >= 0 && r !== old)
        redrawCanvasRow(r);
}
function onScreenMove(ev) {
    if (!storm)
        return setHover(undefined, -1);
    const hit = cellAt(ev);
    const linked = hit !== null && linkHref(screen.links, hit.cell.a) !== null;
    setHover(linked ? hit.cell.a : undefined, linked ? hit.r : -1);
}
function onScreenClick(ev) {
    if (!storm || selectionActive())
        return;
    const hit = cellAt(ev);
    const uri = hit === null ? null : linkHref(screen.links, hit.cell.a);
    if (uri !== null)
        window.open(uri, "_blank", "noopener,noreferrer");
}
function attachLinkHandlers() {
    screenEl.addEventListener("mousemove", onScreenMove);
    screenEl.addEventListener("mouseleave", () => setHover(undefined, -1));
    screenEl.addEventListener("click", onScreenClick);
}
export function ghostText(row) {
    let text = "";
    for (const cell of row)
        text += cell.t && cell.t.length ? cell.t : " ";
    return text;
}
function ghostRow(r) {
    const el = screen.rowEls[r];
    if (!el)
        return;
    el.textContent = ghostText(screen.cells[r] ?? []);
}
let ghostStale = false;
function selectionActive() {
    if (typeof getSelection === "undefined")
        return false;
    const s = getSelection();
    return s !== null && !s.isCollapsed;
}
let ghostCss = false;
function ensureGhostCss() {
    if (ghostCss || typeof document === "undefined")
        return;
    ghostCss = true;
    const st = document.createElement("style");
    st.textContent =
        ".row.ghost{color:transparent;text-shadow:none}" +
            ".row.ghost::selection{background:rgba(110,170,255,.4)}";
    document.head.appendChild(st);
}
function setStorm(on) {
    if (storm === on)
        return;
    storm = on;
    if (!on)
        setHover(undefined, -1);
    for (const { el } of screenImages)
        el.style.visibility = on ? "hidden" : "";
    ensureGhostCss();
    for (const el of screen.rowEls)
        el.classList.toggle("ghost", on);
    if (on) {
        for (let r = 0; r < screen.cells.length; r++)
            ghostRow(r);
        redrawCanvasAll();
        lastStormy = clock();
        stormTimer = setInterval(() => {
            if (clock() - lastStormy > STORM_EXIT_MS && !selectionActive() && !canvasModeOn())
                setStorm(false);
        }, 300);
    }
    else {
        if (stormTimer !== null)
            clearInterval(stormTimer);
        stormTimer = null;
        stormHot = 0;
        for (let r = 0; r < screen.cells.length; r++) {
            const el = screen.rowEls[r];
            if (el)
                el.innerHTML = renderRow(screen.cells[r], cursorCol(screen.cur, r), screen.sty, screen.links);
        }
        redrawCanvasAll();
    }
}
function stormReset() {
    if (stormTimer !== null)
        clearInterval(stormTimer);
    stormTimer = null;
    storm = false;
    stormHot = 0;
}
export function isFillGlyph(cp) {
    return ((cp >= 0xe0b0 && cp <= 0xe0d4) ||
        (cp >= 0x1fb00 && cp <= 0x1fbaf));
}
function symbolFamily(cp) {
    for (const [lo, hi, fam] of cfg.sym) {
        if (cp >= lo && cp <= hi)
            return fam;
    }
    return null;
}
let measCtx = null;
let measOneCh = 0;
const overflowCache = new Map();
function resetGlyphMeasure() {
    measCtx = null;
    measOneCh = 0;
    overflowCache.clear();
}
function glyphOverflowsCell(t, w) {
    if (typeof document === "undefined")
        return false;
    const cp = t.codePointAt(0) ?? 0;
    if (cp >= 0x20 && cp <= 0x7e)
        return false;
    const cached = overflowCache.get(t);
    if (cached !== undefined)
        return cached;
    if (!measCtx) {
        measCtx = document.createElement("canvas").getContext("2d");
        if (!measCtx)
            return false;
        const cs = getComputedStyle(screenEl);
        measCtx.font = `${cs.fontSize} ${cs.fontFamily}`;
        measOneCh = measCtx.measureText("0").width || 1;
    }
    const over = measCtx.measureText(t).width > measOneCh * w * 1.05;
    overflowCache.set(t, over);
    return over;
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
function escAttr(s) {
    return esc(s).replace(/"/g, "&quot;");
}
export function linkHref(links, id) {
    if (id === undefined)
        return null;
    const uri = links[id];
    if (!uri)
        return null;
    return /^(https?|ftp|mailto|file):/i.test(uri) ? uri : null;
}
function symbolSpan(col, w, boxStyle, font, glyph, stretch) {
    const par = stretch ? "none" : "xMidYMid meet";
    const len = stretch ? ' textLength="14" lengthAdjust="spacingAndGlyphs"' : "";
    return (`<span class="run" style="left:${col}ch;width:${w}ch;${boxStyle}">` +
        `<svg viewBox="0 0 14 14" preserveAspectRatio="${par}" style="display:block;width:100%;height:100%">` +
        `<text x="0" y="12" font-family="${font}" font-size="14" fill="currentColor"${len}>${glyph}</text></svg></span>`);
}
function symbolCell(cell, isCursor, col, w, font, deco = "") {
    const boxStyle = cellStyle(cell, isCursor) + deco;
    const t = cell.t ?? " ";
    const cp = t.codePointAt(0) ?? 0x20;
    const stretch = isFillGlyph(cp);
    return symbolSpan(col, w, boxStyle, font, esc(t), stretch);
}
function inkFree(s) {
    return !s.includes("background") && !s.includes("text-decoration") && !s.includes("box-shadow");
}
function cursorDeco(sty) {
    return sty >= 5
        ? "box-shadow:inset 0.14em 0 0 0 currentColor;"
        : "box-shadow:inset 0 -0.14em 0 0 currentColor;";
}
export function renderRow(cells, cursorCol, curSty = 0, links = {}) {
    const blocky = curSty <= 2;
    let out = "";
    let col = 0;
    let runStyle = null;
    let runHref = null;
    let runCol = 0;
    let cols = 0;
    let text = "";
    const flushText = () => {
        if (text.length === 0)
            return;
        const st = `left:${runCol}ch;width:${cols}ch;${runStyle ?? ""}`;
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
        const cp0 = cell.t ? cell.t.codePointAt(0) : 0;
        if (cp0 && isCanvasGlyph(cp0) && !(cp0 >= 0xe000 && symbolFamily(cp0))) {
            flushText();
            runStyle = null;
            runHref = null;
            cols = 0;
            out += `<span class="run" style="left:${col}ch;width:${w}ch;${cellStyle(cell, curBlock)}${deco}color:transparent">${esc(cell.t)}</span>`;
            col += w;
            continue;
        }
        if (cp0 && glyphOverflowsCell(cell.t, w) && !isFillGlyph(cp0) && !symbolFamily(cp0)) {
            flushText();
            runStyle = null;
            runHref = null;
            cols = 0;
            out += `<span class="run" style="left:${col}ch;width:${w}ch;overflow:visible;${cellStyle(cell, curBlock)}${deco}">${esc(cell.t)}</span>`;
            col += w;
            continue;
        }
        const font = svgFont(cell);
        if (font) {
            flushText();
            runStyle = null;
            runHref = null;
            cols = 0;
            out += symbolCell(cell, curBlock, col, w, font, deco);
        }
        else {
            let style = cellStyle(cell, curBlock) + deco;
            const href = linkHref(links, cell.a);
            if ((!cell.t || cell.t === " ") &&
                href === null &&
                runHref === null &&
                runStyle !== null &&
                runStyle !== style &&
                inkFree(style) &&
                inkFree(runStyle)) {
                style = runStyle;
            }
            if (runStyle !== style || runHref !== href) {
                flushText();
                runStyle = style;
                runHref = href;
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
    return out;
}
function cursorCol(cur, row) {
    return cur && cur[0] === row ? cur[1] : -1;
}
let screen = { cells: [], cur: null, sty: 0, links: {}, rowEls: [] };
let screenImages = [];
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
    if (dp.sty !== undefined && dp.sty !== (state.sty ?? 0)) {
        state.sty = dp.sty;
        if (state.cur)
            dirty.add(state.cur[0]);
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
let paintScheduled = false;
const dirtyRows = new Set();
let rebuildDims = null;
let rebuildBanner = null;
let lastFlush = 0;
const TARGET_LOAD = 0.7;
const MAX_INTERVAL = 250;
let paintCost = 16;
let bytesIn = 0;
let paints = 0;
const raf = (cb) => (typeof requestAnimationFrame !== "undefined" ? requestAnimationFrame : (f) => setTimeout(f, 16))(cb);
const clock = () => (typeof performance !== "undefined" ? performance.now() : 0);
function schedulePaint() {
    if (paintScheduled)
        return;
    paintScheduled = true;
    raf(flushPaint);
}
function flushPaint() {
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
        stormReset();
        screenEl.innerHTML = rebuildBanner;
        rebuildBanner = null;
        rebuildDims = null;
        dirtyRows.clear();
        return;
    }
    const t0 = clock();
    if (rebuildDims) {
        stormReset();
        paintFull(rebuildDims);
        rebuildDims = null;
        dirtyRows.clear();
        if (canvasModeOn())
            setStorm(true);
        lastCurPos = screen.cur ? [screen.cur[0], screen.cur[1]] : null;
    }
    else {
        const stormy = dirtyRows.size >= STORM_RATIO * (screen.cells.length || 1);
        if (stormy) {
            lastStormy = now;
            if (stormAutoOn() && !storm && ++stormHot >= STORM_ENTER)
                setStorm(true);
        }
        else if (!storm) {
            stormHot = 0;
        }
        const cur = screen.cur;
        if (storm && smoothCursorOn() && cur && lastCurPos &&
            (cur[0] !== lastCurPos[0] || cur[1] !== lastCurPos[1])) {
            startCurAnim(lastCurPos, cur);
        }
        lastCurPos = cur ? [cur[0], cur[1]] : null;
        const frozen = storm && selectionActive();
        if (storm && !frozen && ghostStale) {
            for (let r = 0; r < screen.cells.length; r++)
                ghostRow(r);
            ghostStale = false;
        }
        for (const r of dirtyRows) {
            if (storm) {
                drawRowStorm(r);
                if (frozen)
                    ghostStale = true;
                else
                    ghostRow(r);
            }
            else {
                const el = screen.rowEls[r];
                if (!el)
                    continue;
                el.innerHTML = renderRow(screen.cells[r] ?? [], cursorCol(screen.cur, r), screen.sty, screen.links);
                redrawCanvasRow(r);
            }
        }
        dirtyRows.clear();
    }
    raf(() => {
        paintCost += 0.3 * (clock() - t0 - paintCost);
    });
}
function applyFull(m) {
    screen = {
        cells: m.d.map(decodeBlock),
        cur: m.p ?? null,
        sty: m.q ?? 0,
        links: m.y ?? {},
        rowEls: [],
    };
    defaultsCss = applyDefaults(m.e);
    setTitle(m.t ?? "");
    rebuildDims = { w: m.w, h: m.h, i: m.i };
    rebuildBanner = null;
    dirtyRows.clear();
    schedulePaint();
}
let defaultsCss = { fg: "", bg: "" };
let bootTitle = null;
function setTitle(t) {
    if (typeof document === "undefined")
        return;
    if (bootTitle === null)
        bootTitle = document.title;
    document.title = t || bootTitle;
}
function paintFull(dims) {
    screenEl.style.color = defaultsCss.fg;
    screenEl.style.backgroundColor = defaultsCss.bg;
    const cur = screen.cur;
    let html = `<div class="screen" style="width:${dims.w}ch;height:calc(${dims.h} * var(--lh));">`;
    for (let r = 0; r < screen.cells.length; r++) {
        html += `<div class="row">${renderRow(screen.cells[r], cursorCol(cur, r), screen.sty, screen.links)}</div>`;
    }
    html += "</div>";
    screenEl.innerHTML = html;
    const screenDiv = screenEl.firstElementChild;
    screen.rowEls = Array.from(screenDiv.children);
    attachCanvas(dims.w, dims.h, screenDiv);
    redrawCanvasAll();
    if (dims.i?.length)
        screenDiv.insertAdjacentHTML("beforeend", renderImages(dims.i));
    screenImages = (dims.i ?? []).map((ref, idx) => ({
        ref,
        el: screenDiv.querySelectorAll("img.inline-img")[idx],
    }));
    for (const { el } of screenImages) {
        el.style.visibility = storm ? "hidden" : "";
        if (!el.complete)
            el.addEventListener("load", () => {
                if (storm)
                    redrawCanvasAll();
            });
    }
}
function renderImages(imgs) {
    return imgs
        .map((im) => {
        const size = im.w && im.h ? `width:${im.w}ch;height:calc(${im.h} * var(--lh));object-fit:contain;object-position:left top;` : "";
        return `<img class="inline-img" alt="" src="data:${im.m};base64,${im.d}" style="position:absolute;left:${im.c}ch;top:calc(${im.r} * var(--lh));${size}z-index:3;pointer-events:none;">`;
    })
        .join("");
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
function applyPatches(cur, sty, rows) {
    const dirty = patchCells(screen, { cur, sty, rows });
    for (const r of dirty)
        dirtyRows.add(r);
    schedulePaint();
}
function applyDiff(m) {
    if (m.t !== undefined)
        setTitle(m.t);
    if (m.y)
        Object.assign(screen.links, m.y);
    applyPatches(m.p, m.q, (m.r ?? []).map(decodeRow));
}
function applyCell(m) {
    const { c: r, p: _p, q: _q, ...style } = m;
    const styled = Object.keys(style).length > 0;
    const cells = [];
    for (const ch of r[2])
        cells.push(styled ? { t: ch, ...style } : { t: ch });
    applyPatches(m.p, m.q, [{ r: r[0], l: r[1], cells }]);
}
function applyLine(m) {
    applyPatches(m.p, m.q, [decodeRow(m.l)]);
}
function applyBanner(m) {
    screen = { cells: [], cur: null, sty: 0, links: {}, rowEls: [] };
    rebuildBanner = m.b;
    rebuildDims = null;
    dirtyRows.clear();
    schedulePaint();
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
let sseDown = false;
let operatorDown = false;
function refreshLive() {
    const state = sseDown ? "hub" : operatorDown ? "operator" : "";
    if (state)
        document.body.dataset.offline = state;
    else
        delete document.body.dataset.offline;
}
function connect(events) {
    const es = new EventSource(events);
    es.onopen = () => {
        sseDown = false;
        refreshLive();
    };
    es.onmessage = (e) => {
        bytesIn += e.data.length;
        apply(JSON.parse(e.data));
    };
    es.addEventListener("operator", (e) => {
        operatorDown = e.data === "0";
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
function fmtRate(bytesPerSec) {
    if (bytesPerSec >= 1e6)
        return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
    if (bytesPerSec >= 1e3)
        return `${(bytesPerSec / 1e3).toFixed(0)} KB/s`;
    return `${bytesPerSec.toFixed(0)} B/s`;
}
function startStats() {
    const el = document.getElementById("sg-stats");
    if (!el)
        return;
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
function injectViewerCss() {
    const linkCss = document.createElement("style");
    linkCss.textContent =
        "#screen a.run{color:inherit;text-decoration:none}" +
            "#screen a.run:hover{text-decoration:underline}";
    document.head.appendChild(linkCss);
}
export function benchInit(el) {
    screenEl = el;
    injectViewerCss();
    attachLinkHandlers();
}
export function benchStats() {
    return { paints, cost: paintCost, storm };
}
export function benchStorm(on) {
    setStorm(on);
}
export function benchFlush() {
    flushPaint();
}
export function benchCursorStep() {
    stepCurAnim();
}
function main() {
    const boot = window.SHELLGLASS;
    setConfig(boot.cfg);
    setProto(boot.proto, boot.js);
    screenEl = document.getElementById("screen");
    injectViewerCss();
    attachLinkHandlers();
    connect(boot.events);
    startStats();
    const reflowGlyphs = () => {
        resetGlyphMeasure();
        sizeCanvas();
        redrawCanvasAll();
        for (let r = 0; r < screen.cells.length; r++)
            dirtyRows.add(r);
        schedulePaint();
    };
    document.fonts?.addEventListener("loadingdone", reflowGlyphs);
    document.fonts?.ready.then(reflowGlyphs);
}
if (typeof document !== "undefined" && window.SHELLGLASS) {
    main();
}
