"""A small API, web frontend and worker; Python 3.9+ standard library only."""
import argparse
import faulthandler
import json
import os
from pathlib import Path
import signal
import socketserver
import threading
from http.client import HTTPException
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import URLError
from urllib.request import Request, urlopen


PAGE = b"""<!doctype html>
<html lang="en"><meta charset="utf-8"><title>devd local stack</title>
<style>body{font:18px system-ui;max-width:42em;margin:10vh auto;padding:1em}
pre{padding:1em;background:#eee;border-radius:8px}</style>
<h1>Your local stack is running.</h1>
<p>The worker submits jobs to the API. This frontend reads its status.</p>
<pre id="status">Connecting...</pre>
<script>async function refresh(){try{const r=await fetch('/status');
document.getElementById('status').textContent=JSON.stringify(await r.json(),null,2);
}catch(e){document.getElementById('status').textContent=String(e)}}
refresh();setInterval(refresh,1000);</script></html>"""


class LocalThreadingHTTPServer(ThreadingHTTPServer):
    """Bind the fixture without a reverse DNS lookup on the loopback host."""

    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = self.server_address[0]
        self.server_port = self.server_address[1]


def main():
    if os.environ.get("DEVD_SMOKE_DIAGNOSTICS"):
        faulthandler.dump_traceback_later(3, repeat=True)
    parser = argparse.ArgumentParser()
    parser.add_argument("role", choices=["api", "web", "worker"])
    parser.add_argument("--port", type=int, default=8731)
    parser.add_argument("--api", default="http://127.0.0.1:8731")
    args = parser.parse_args()
    stop = threading.Event()
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    if args.role == "worker":
        faulthandler.cancel_dump_traceback_later()
        print("worker started", flush=True)
        while not stop.is_set():
            try:
                with urlopen(Request(args.api + "/jobs", data=b"", method="POST"), timeout=1) as response:
                    print("job completed: " + response.read().decode(), flush=True)
            except (HTTPException, URLError, OSError) as error:
                print(f"API unavailable; will retry: {error}", flush=True)
            stop.wait(1)
        print("worker stopped", flush=True)
        return

    count = 0
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, format, *values):
            print(format % values, flush=True)

        def send(self, status, body, content_type="application/json"):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/health":
                unhealthy = Path("unhealthy").exists() and args.role == "api"
                self.send(503 if unhealthy else 200, b'{"healthy":false}' if unhealthy else b'{"healthy":true}')
            elif self.path == "/status" and args.role == "api":
                with lock:
                    body = json.dumps({"pid": os.getpid(), "completed_jobs": count}).encode()
                self.send(200, body)
            elif self.path == "/status" and args.role == "web":
                try:
                    with urlopen(args.api + "/status", timeout=1) as response:
                        self.send(200, response.read())
                except (URLError, OSError):
                    self.send(503, b'{"error":"API unavailable; retrying on next refresh"}')
            elif self.path == "/" and args.role == "web":
                self.send(200, PAGE, "text/html; charset=utf-8")
            else:
                self.send(404, b'{"error":"not found"}')

        def do_POST(self):
            nonlocal count
            if args.role == "api" and self.path == "/jobs":
                with lock:
                    count += 1
                    body = json.dumps({"completed_jobs": count}).encode()
                self.send(200, body)
            else:
                self.send(404, b'{"error":"not found"}')

    with LocalThreadingHTTPServer(("127.0.0.1", args.port), Handler) as server:
        faulthandler.cancel_dump_traceback_later()
        server.timeout = 0.2
        print(f"{args.role} listening at http://127.0.0.1:{server.server_port}", flush=True)
        while not stop.is_set():
            server.handle_request()
    print(f"{args.role} stopped", flush=True)


if __name__ == "__main__":
    main()
