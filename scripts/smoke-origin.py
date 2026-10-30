#!/usr/bin/env python3
"""A tiny origin for scripts/image-smoke.sh: the witness for the image's origin ceiling.

Standard library only, because it runs on a bare release runner before any
toolchain is installed. It counts the renders it is doing itself, so the smoke
test measures the origin's concurrency rather than reading the proxy's own
gauges, and it decrements before writing the last byte of a response, so a
proxy that releases its permit on reading that byte never finds a finished
render still counted (the same ordering as bench/slow-origin).

  GET /healthz         200 "ok"
  GET /hello           200 "hello from the smoke origin"
  GET /slow/<ms>       200 after <ms> milliseconds, Cache-Control: no-store
  GET /__witness       {"renders": n, "peak": n, "active": n}, not counted
"""
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
STATE = {"renders": 0, "peak": 0, "active": 0}


class Origin(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, body, cache_control="no-store"):
        self.send_response(status)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Cache-Control", cache_control)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path == "/__witness":
            with LOCK:
                body = json.dumps(STATE).encode()
            return self.reply(200, body)
        if path == "/healthz":
            return self.reply(200, b"ok")
        if path == "/hello":
            return self.reply(200, b"hello from the smoke origin")
        if path.startswith("/slow/"):
            delay = int(path.removeprefix("/slow/")) / 1000
            with LOCK:
                STATE["renders"] += 1
                STATE["active"] += 1
                STATE["peak"] = max(STATE["peak"], STATE["active"])
            try:
                time.sleep(delay)
                body = b"rendered"
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body[:-1])
            finally:
                with LOCK:
                    STATE["active"] -= 1
            self.wfile.write(body[-1:])
            return None
        return self.reply(404, b"not found")


def main():
    port = int(sys.argv[1])
    server = ThreadingHTTPServer(("127.0.0.1", port), Origin)
    server.daemon_threads = True
    server.serve_forever()


if __name__ == "__main__":
    main()
