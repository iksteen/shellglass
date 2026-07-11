#!/usr/bin/env python3
"""Canvas verification: screenshot verify.html (the canvas picture over ghost
text) and check terminal-rendering semantics on the pixels, plus the green/red
per-mode self-checks. Kills only the Firefox PIDs it spawns."""
import base64, http.server, os, socketserver, subprocess, sys, threading
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
LH = 17  # --lh in verify.html (CSS px; screenshots at dpr=1 headless)
ROWS, COLS, FS = 17, 60, 14

# A red 8x8 PNG, served for any /images/<hash> request (the ?mode=image
# self-check references a fixed content address; in production the shellglass
# servers own this route).
RED8 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAEklEQVR4nGP8z4AdMOEQ"
    "H6QSAM1BAQ/oQeJvAAAAAElFTkSuQmCC"
)

class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=HERE, **kw)
    def do_GET(self):
        if self.path.split("?")[0].startswith("/images/"):
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(RED8)))
            self.end_headers()
            self.wfile.write(RED8)
            return
        super().do_GET()
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
        base = "/tmp/sg-verify-static.png"
        shot(port, "static", base)
        selfchecks = ["links", "crt", "image", "cursor&cursor=smooth", "bleed",
                      "blink", "freeze", "hold", "weight", "liga", "pinch"]
        paths = {}
        for m in selfchecks:
            name = m.split("&")[0]
            paths[name] = f"/tmp/sg-verify-{name}.png"
            shot(port, m, paths[name])
        srv.shutdown()
    img = Image.open(base).convert("RGB")
    # Every content row must have ink in its own band (baseline sanity: text
    # is seated in its row box, nothing amputated or shifted a band away).
    print(f"{'row':>3} {'ink-y':>7} {'ink':>9}")
    empty = []
    for r in range(ROWS):
        cy, ink = band_profile(img, r)
        if cy < 0:
            empty.append(r)
            continue
        print(f"{r:>3} {cy:>7.2f} {ink:>9.0f}")
    # Row 11 is the concealed row — the ONLY row allowed (required) to be dark.
    print(f"row-ink check: {'PASS' if empty == [11] else f'FAIL (dark rows {empty})'}")
    # Device-pixel bg continuity: row 10 is a 50-cell contiguous bg run; a
    # hairline seam shows as a column whose summed brightness dips far below
    # its neighbours inside the run.
    BGROW = 10
    px = img.load()
    y0, y1 = BGROW * LH, (BGROW + 1) * LH
    cols = []
    for x in range(0, int(50 * 8.4)):  # stay inside the 50-cell run
        v = sum(sum(px[x, y][:3]) for y in range(y0, y1))
        cols.append(v)
    med = sorted(cols)[len(cols) // 2]
    seams = sum(1 for v in cols if v < med * 0.5)
    print(f"bg seam check: {'PASS' if seams == 0 else f'FAIL ({seams} dark cols)'}")
    # Conceal (SGR 8): row 11 is concealed text — the glyphs must not render
    # (the one direction mirror fidelity can leak content).
    ink = sum(
        1
        for y in range(11 * LH, 12 * LH)
        for x in range(0, int(18 * 8.4))
        if sum(px[x, y][:3]) > 60
    )
    print(f"conceal check: {'PASS' if ink == 0 else f'FAIL ({ink} lit px)'}")
    # Underline skip-ink (D.4): row 5's underline must PART around the
    # descenders of "Mgjq" (cols 14-17) — kitty's exclusion zones. Gap = a
    # column in the underline band with no ink below the x-height region.
    y0, y1 = 5 * LH + 12, 6 * LH  # the underline band (below the baseline)
    gaps = sum(
        1
        for x in range(int(14 * 8.4), int(18 * 8.4))
        if all(sum(px[x, y][:3]) <= 60 for y in range(y0, y1))
    )
    print(f"skip-ink check: {'PASS' if gaps >= 2 else f'FAIL ({gaps} gap cols)'}")
    # Decoration continuity at SPACES: rows 5 (underline) and 9 (strike)
    # style their first 18 cells including the spaces between words — the
    # line must run through the space cells (a terminal decorates the cell,
    # not the glyph). Only space cells are scanned; over glyphs the skip-ink
    # exclusion above legitimately parts the line.
    gaps = []
    for r, cols_ in ((5, (9, 13)), (9, (13,))):
        y0, y1 = r * LH, (r + 1) * LH
        for col in cols_:
            for x in range(int(col * 8.4) + 2, int((col + 1) * 8.4) - 2):
                if all(sum(px[x, y][:3]) <= 60 for y in range(y0, y1)):
                    gaps.append((r, x))
    print(f"decoration continuity: "
          f"{'PASS' if not gaps else f'FAIL ({len(gaps)} empty cols, first {gaps[0]})'}")
    for name, path in paths.items():
        lr, lg, lb = Image.open(path).convert("RGB").load()[20, 20]
        print(f"{name} self-check: {'PASS' if lg > 200 and lr < 60 else 'FAIL'}")

main()
