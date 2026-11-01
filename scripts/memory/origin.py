"""Bounded local origin used by the Issue 34 soak.

GET responses are deterministic; POST bodies are echoed so the TLS/h2 verifier
can prove the payload crossed the proxy unchanged. The server keeps no
per-request state beyond the request currently being served.
"""

from __future__ import annotations

import hashlib
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


def deterministic_payload(size: int) -> bytes:
    if size <= 0:
        raise ValueError("payload size must be positive")
    unit = b"zelvik-issue34-origin\n"
    repeats = (size + len(unit) - 1) // len(unit)
    return (unit * repeats)[:size]


def payload_sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    payload = deterministic_payload(16 * 1024)

    def log_message(self, fmt, *args):  # noqa: A003 - stdlib signature
        # Keep CI logs bounded; the soak records counters itself.
        return

    def _send(self, status: int, body: bytes) -> None:
        self.send_response(status)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):  # noqa: N802 - stdlib signature
        if self.path == "/healthz":
            self._send(200, b"ok\n")
        else:
            self._send(200, self.payload)

    def do_POST(self):  # noqa: N802 - stdlib signature
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send(400, b"bad content-length\n")
            return
        if length <= 0 or length > 8 * 1024 * 1024:
            self._send(413, b"bounded payload required\n")
            return
        body = self.rfile.read(length)
        if len(body) != length:
            self._send(400, b"short body\n")
            return
        self._send(200, body)


class OriginServer:
    def __init__(self, host: str = "127.0.0.1", port: int = 0, payload_bytes: int = 16 * 1024):
        _Handler.payload = deterministic_payload(payload_bytes)
        self._server = ThreadingHTTPServer((host, port), _Handler)
        self._server.daemon_threads = True
        self._thread = threading.Thread(
            target=self._server.serve_forever, name="issue34-origin", daemon=True
        )

    @property
    def port(self) -> int:
        return int(self._server.server_address[1])

    def start(self) -> "OriginServer":
        self._thread.start()
        return self

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=10)
