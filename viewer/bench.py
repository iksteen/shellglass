#!/usr/bin/env python3
"""exp/cell-matrix bench driver: serves viewer/ + dist/viewer.js, launches
headless Firefox at bench.html for each mode/size, collects the POSTed
results, prints a table. Kills only the Firefox PIDs it spawned."""
import http.server, json, os, socketserver, subprocess, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
RESULTS = []
DONE = threading.Event()

class H(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **kw):
        super().__init__(*a, directory=HERE, **kw)
    def translate_path(self, path):
        # bench.html imports ./viewer.js — serve the committed dist build.
        if path.split("?")[0] == "/viewer.js":
            return os.path.join(HERE, "dist", "viewer.js")
        return super().translate_path(path)
    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        RESULTS.append(json.loads(self.rfile.read(n)))
        self.send_response(204); self.end_headers()
        DONE.set()
    def log_message(self, *a): pass

def run(port, mode, load, cols, rows, secs):
    DONE.clear()
    url = f"http://127.0.0.1:{port}/bench.html?mode={mode}&load={load}&cols={cols}&rows={rows}&secs={secs}"
    profile = f"/tmp/sg-bench-profile-{mode}-{cols}x{rows}"
    p = subprocess.Popen(
        ["firefox", "--headless", "--no-remote", "--profile", profile, url],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    ok = DONE.wait(timeout=secs + 60)
    p.terminate()  # only the PID we spawned
    try: p.wait(timeout=10)
    except subprocess.TimeoutExpired: p.kill()
    return ok

def main():
    secs = int(sys.argv[1]) if len(sys.argv) > 1 else 5
    with socketserver.TCPServer(("127.0.0.1", 0), H) as srv:
        port = srv.server_address[1]
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        for load in ["typing", "editor", "rain"]:
            for cols, rows in [(80, 24), (200, 60), (320, 100)]:
                for mode in ["dom-nostorm", "dom", "canvas"]:
                    os.makedirs(f"/tmp/sg-bench-profile-{mode}-{cols}x{rows}", exist_ok=True)
                    if not run(port, mode, load, cols, rows, secs):
                        print(f"{mode} {load} {cols}x{rows}: TIMED OUT", file=sys.stderr)
        srv.shutdown()
    print(f"{'load':<8} {'size':<10} {'mode':<9} {'fps':>7} {'flush ms':>9} {'storm':>6}")
    for r in RESULTS:
        print(f"{r.get('load','rain'):<8} {r['cols']}x{r['rows']:<5} {r['mode']:<9} "
              f"{r['fps']:>7.1f} {r['cost_ms']:>9.2f} {str(r['storm']):>6}")

main()
