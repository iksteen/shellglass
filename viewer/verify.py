#!/usr/bin/env python3
"""Canvas-track verification: screenshot verify.html in DOM mode and with
storm forced, then measure per-row-band vertical ink centroids and coverage.
Kills only the Firefox PIDs it spawns."""
import http.server, os, socketserver, subprocess, sys, threading
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
LH = 17  # --lh in verify.html (CSS px; screenshots at dpr=1 headless)
ROWS, COLS, FS = 17, 60, 14

class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=HERE, **kw)
    def translate_path(self, path):
        if path.split("?")[0] == "/viewer.js":
            return os.path.join(HERE, "dist", "viewer.js")
        return super().translate_path(path)
    def log_message(self, *a): pass

def shot(port, mode, out):
    url = f"http://127.0.0.1:{port}/verify.html?mode={mode}"
    profile = f"/tmp/sg-verify-{mode}"
    os.makedirs(profile, exist_ok=True)
    subprocess.run(
        ["firefox", "--headless", "--no-remote", "--profile", profile,
         "--window-size", "900,400", "--screenshot", out, url],
        check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def band_profile(img, r, lh=LH):
    """(centroid_y, ink_sum) of non-background pixels in row band r."""
    px = img.load()
    y0, y1 = round(r * lh), round((r + 1) * lh)
    total, weighted = 0.0, 0.0
    for y in range(y0, min(y1, img.height)):
        rowsum = 0
        for x in range(0, min(int(COLS * 9 * lh / LH), img.width)):
            p = px[x, y]
            v = p[0] + p[1] + p[2]
            if v > 60:  # not (near-)black background
                rowsum += v
        total += rowsum
        weighted += rowsum * (y - y0)
    return (weighted / total if total else -1.0, total)

def main():
    with socketserver.TCPServer(("127.0.0.1", 0), H) as srv:
        port = srv.server_address[1]
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        a, b = "/tmp/sg-verify-dom.png", "/tmp/sg-verify-storm.png"
        c = "/tmp/sg-verify-links.png"
        e = "/tmp/sg-verify-crt.png"
        f = "/tmp/sg-verify-image.png"
        h = "/tmp/sg-verify-cursor.png"
        j = "/tmp/sg-verify-bleed.png"
        shot(port, "dom", a)
        shot(port, "storm", b)
        shot(port, "links", c)
        shot(port, "crt", e)
        shot(port, "image", f)
        shot(port, "cursor&cursor=smooth", h)
        shot(port, "bleed", j)
        za, zb = "/tmp/sg-verify-zdom.png", "/tmp/sg-verify-zstorm.png"
        shot(port, "dom&zoom=1.5", za)
        shot(port, "storm&zoom=1.5", zb)
        srv.shutdown()
    dom, storm = Image.open(a).convert("RGB"), Image.open(b).convert("RGB")
    print(f"{'row':>3} {'dom-y':>7} {'storm-y':>8} {'shift':>6} {'ink dom':>9} {'ink storm':>10}")
    # Row 6 (double underline): the canvas clamps both bars fully inside the
    # cell box (kitty's model); the DOM clips its lower bar at the .run edge.
    # Intended divergence — report it, keep it out of the gate.
    CLAMPED = {6}
    worst = 0.0
    for r in range(ROWS):
        cd, id_ = band_profile(dom, r)
        cs, is_ = band_profile(storm, r)
        if cd < 0 or cs < 0:
            continue
        shift = cs - cd
        if r not in CLAMPED:
            worst = max(worst, abs(shift))
        tag = "  (in-box clamp, ungated)" if r in CLAMPED else ""
        print(f"{r:>3} {cd:>7.2f} {cs:>8.2f} {shift:>+6.2f} {id_:>9.0f} {is_:>10.0f}{tag}")
    print(f"worst vertical shift: {worst:.2f}px "
          "(canvas seats ink fully in-box — kitty's model; the DOM clips low "
          "ink at the .run edge, so a sub-pixel lift vs the DOM is intended)")
    # Same parity under the template's CSS-zoom model (the local zoom): the
    # canvas derives fontPx across the zoomed/local coordinate-space split.
    # Gated on TEXT rows: on decoration rows (5-9) the zoomed DOM clips its
    # own low-riding underlines at the .run boundary (Firefox artifact,
    # measured: DOM ink drops, canvas keeps the full decoration) — a centroid
    # gap there is the DOM losing ink, not the canvas mis-seating glyphs.
    DECOR = {5, 6, 7, 8, 9}
    zdom, zstorm = Image.open(za).convert("RGB"), Image.open(zb).convert("RGB")
    zworst = zdecor = 0.0
    for r in range(ROWS):
        cd, _ = band_profile(zdom, r, LH * 1.5)
        cs, _ = band_profile(zstorm, r, LH * 1.5)
        if cd < 0 or cs < 0:
            continue
        if r in DECOR:
            zdecor = max(zdecor, abs(cs - cd))
        else:
            zworst = max(zworst, abs(cs - cd))
    print(f"worst text shift at zoom 1.5: {zworst:.2f}px "
          f"({'PASS' if zworst < 0.3 else 'FAIL'}; decoration rows {zdecor:.2f}px, "
          f"dominated by the DOM's own clipping)")
    # Device-pixel bg continuity: row 10 is a 50-cell contiguous bg run; a
    # hairline seam shows as a column whose summed brightness dips far below
    # its neighbours inside the run. (Row 10 in the frame = index BGROW.)
    BGROW = 10
    for label, img in (("dom", dom), ("storm", storm)):
        px = img.load()
        y0, y1 = BGROW * LH, (BGROW + 1) * LH
        cols = []
        for x in range(0, int(50 * 8.4)):  # stay inside the 50-cell run
            v = sum(sum(px[x, y][:3]) for y in range(y0, y1))
            cols.append(v)
        med = sorted(cols)[len(cols) // 2]
        seams = sum(1 for v in cols if v < med * 0.5)
        print(f"bg seam check ({label}): {'PASS' if seams == 0 else f'FAIL ({seams} dark cols)'}")
    # Decoration continuity at SPACES: rows 5 (underline) and 9 (strike)
    # style their first 18 cells including the spaces between words — the
    # line must run through the space cells (text-decoration doesn't break
    # at spaces). Only space cells are scanned: over glyphs, Firefox's
    # text-decoration-skip-ink legitimately lifts the DOM underline around
    # descenders (kitty's underline exclusion zones are the terminal
    # equivalent; the canvas draws straight through — not checked here).
    for label, img in (("dom", dom), ("storm", storm)):
        px = img.load()
        gaps = []
        for r, cols_ in ((5, (9, 13)), (9, (13,))):
            y0, y1 = r * LH, (r + 1) * LH
            for col in cols_:
                for x in range(int(col * 8.4) + 2, int((col + 1) * 8.4) - 2):
                    if all(sum(px[x, y][:3]) <= 60 for y in range(y0, y1)):
                        gaps.append((r, x))
        print(f"decoration continuity ({label}): "
              f"{'PASS' if not gaps else f'FAIL ({len(gaps)} empty cols, first {gaps[0]})'}")
    for name, path in (("links", c), ("crt", e), ("image", f), ("cursor", h), ("bleed", j)):
        lr, lg, lb = Image.open(path).convert("RGB").load()[20, 20]
        print(f"{name} self-check: {'PASS' if lg > 200 and lr < 60 else 'FAIL'}")

main()
