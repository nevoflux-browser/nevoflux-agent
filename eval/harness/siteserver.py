"""Local site server for eval trials: static files + an event log.

Pages report what the agent did through /__event (see sites/_lib/beacon.js);
the grader reads the log instead of scraping the page after the browser is
gone. Events are scoped to the current trial so a late beacon from the
previous trial can never count.
"""

import json
import pathlib
import threading
import time
import urllib.parse
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class SiteServer:
    def __init__(self, root: pathlib.Path, port: int = 0):
        self.root = pathlib.Path(root)
        self.port = port
        self._trial = ""
        self._events: list[dict] = []
        self._lock = threading.Lock()
        self._httpd = None
        self._thread = None

    def set_trial(self, trial_id: str) -> None:
        with self._lock:
            self._trial = trial_id

    def events(self) -> list[dict]:
        with self._lock:
            return [e for e in self._events if e["trial"] == self._trial]

    def _record(self, site: str, kind: str, data) -> None:
        with self._lock:
            self._events.append({"trial": self._trial, "ts": time.time(),
                                 "site": site, "kind": kind, "data": data})

    def _reset(self) -> None:
        with self._lock:
            self._events.clear()

    def start(self) -> int:
        server = self

        class Handler(SimpleHTTPRequestHandler):
            def __init__(self, *a, **kw):
                super().__init__(*a, directory=str(server.root), **kw)

            def log_message(self, *_):
                pass

            def end_headers(self):
                self.send_header("Cache-Control", "no-store")
                super().end_headers()

            def _json(self, code, obj):
                body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b"{}"
                path = urllib.parse.urlparse(self.path).path
                if path == "/__event":
                    try:
                        msg = json.loads(raw.decode("utf-8") or "{}")
                    except json.JSONDecodeError:
                        self.send_response(400)
                        self.end_headers()
                        return
                    server._record(str(msg.get("site", "")), str(msg.get("kind", "")), msg.get("data", {}))
                    self.send_response(204)
                    self.end_headers()
                elif path == "/__reset":
                    server._reset()
                    self.send_response(204)
                    self.end_headers()
                else:
                    self.send_response(404)
                    self.end_headers()

            def do_GET(self):
                u = urllib.parse.urlparse(self.path)
                if u.path == "/__events":
                    return self._json(200, server.events())
                if u.path == "/__slow":
                    q = urllib.parse.parse_qs(u.query)
                    time.sleep(int(q.get("ms", ["1000"])[0]) / 1000)
                    server._record(q.get("site", [""])[0], q.get("kind", ["slow"])[0], {})
                    return self._json(200, {"ok": True})
                return super().do_GET()

        self._httpd = ThreadingHTTPServer(("127.0.0.1", self.port), Handler)
        self.port = self._httpd.server_address[1]
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)
        self._thread.start()
        return self.port

    def stop(self) -> None:
        if self._httpd:
            self._httpd.shutdown()
            self._httpd.server_close()


if __name__ == "__main__":
    # Manual browsing: python -m eval.harness.siteserver [port]
    import sys

    s = SiteServer(pathlib.Path(__file__).resolve().parent / "sites",
                   int(sys.argv[1]) if len(sys.argv) > 1 else 18080)
    s.set_trial("manual")
    print(f"serving http://127.0.0.1:{s.start()}/  (events: /__events)")
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        s.stop()
