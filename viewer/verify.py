#!/usr/bin/env python3
"""Canvas-track verification: screenshot verify.html in DOM mode and with
storm forced, then measure per-row-band vertical ink centroids and coverage.
Kills only the Firefox PIDs it spawns."""
import http.server, os, socketserver, subprocess, sys, threading
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
LH = 17  # --lh in verify.html (CSS px; screenshots at dpr=1 headless)
ROWS, COLS, FS = 12, 60, 14

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

def band_profile(img, r):
    """(centroid_y, ink_sum) of non-background pixels in row band r."""
    px = img.load()
    y0, y1 = r * LH, (r + 1) * LH
    total, weighted = 0.0, 0.0
    for y in range(y0, min(y1, img.height)):
        rowsum = 0
        for x in range(0, min(COLS * 9, img.width)):
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
        shot(port, "dom", a)
        shot(port, "storm", b)
        srv.shutdown()
    dom, storm = Image.open(a).convert("RGB"), Image.open(b).convert("RGB")
    print(f"{'row':>3} {'dom-y':>7} {'storm-y':>8} {'shift':>6} {'ink dom':>9} {'ink storm':>10}")
    worst = 0.0
    for r in range(ROWS):
        cd, id_ = band_profile(dom, r)
        cs, is_ = band_profile(storm, r)
        if cd < 0 or cs < 0:
            continue
        shift = cs - cd
        worst = max(worst, abs(shift))
        print(f"{r:>3} {cd:>7.2f} {cs:>8.2f} {shift:>+6.2f} {id_:>9.0f} {is_:>10.0f}")
    print(f"worst vertical shift: {worst:.2f}px")

main()
