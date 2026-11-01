"""Bounded concurrent HTTP/1.1 keep-alive load for the Issue 34 soak.

HTTP/2 correctness is verified periodically by :mod:`verify_tls` with curl.
The bulk load intentionally uses the standard library HTTP/1.1 client: its
memory and thread counts are bounded by ``concurrency`` and its latency
reservoir is a fixed-size deque, so the generator itself cannot grow without
limit. A worker that sees an unexpected status, a short body or a digest
mismatch records a bounded error sample and reconnects.
"""

from __future__ import annotations

import http.client
import socket
import ssl
import threading
import time
from collections import deque
from dataclasses import dataclass, field

MAX_CONCURRENCY = 1024
MAX_ERROR_SAMPLES = 32
DEFAULT_LATENCY_RESERVOIR = 4096


class LoadGenError(RuntimeError):
    """The load generator configuration is invalid or it could not start."""


@dataclass(frozen=True)
class LoadConfig:
    sni: str
    port: int
    ca_file: str
    path: str = "/issue34/payload"
    concurrency: int = 8
    duration_seconds: float = 60.0
    max_requests: int = 0
    method: str = "GET"
    payload: bytes = b""
    timeout_seconds: float = 10.0
    latency_reservoir: int = DEFAULT_LATENCY_RESERVOIR

    def validate(self) -> None:
        if not self.sni:
            raise LoadGenError("SNI host is required")
        if not (1 <= self.port <= 65535):
            raise LoadGenError(f"invalid port {self.port}")
        if not self.ca_file:
            raise LoadGenError("CA file is required")
        if not (1 <= self.concurrency <= MAX_CONCURRENCY):
            raise LoadGenError(f"concurrency must be 1..{MAX_CONCURRENCY}")
        if self.duration_seconds <= 0:
            raise LoadGenError("duration must be positive")
        if self.max_requests < 0:
            raise LoadGenError("max_requests must be >= 0")
        if self.method not in ("GET", "POST"):
            raise LoadGenError(f"unsupported method {self.method!r}")
        if self.method == "POST" and not self.payload:
            raise LoadGenError("POST load requires a payload")
        if self.latency_reservoir < 16:
            raise LoadGenError("latency reservoir must be at least 16")


class _SNIConnection(http.client.HTTPConnection):
    """Connects to loopback while sending the requested DNS name as SNI/Host."""

    def __init__(self, sni: str, port: int, context: ssl.SSLContext, timeout: float):
        super().__init__(sni, port, timeout=timeout)
        self._sni = sni
        self._context = context

    def connect(self) -> None:  # noqa: D401 - stdlib override
        sock = socket.create_connection(("127.0.0.1", self.port), self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self._sni)


@dataclass
class _Counters:
    ok: int = 0
    errors: int = 0
    bytes: int = 0


class LoadRunner:
    def __init__(self, config: LoadConfig):
        config.validate()
        self.config = config
        self._stop = threading.Event()
        self._threads: list[threading.Thread] = []
        self._lock = threading.Lock()
        self._counters = _Counters()
        self._reservoir: deque[float] = deque(maxlen=config.latency_reservoir)
        self._error_samples: list[str] = []
        self._started = 0.0
        self._finished = 0.0
        self._context = ssl.create_default_context(cafile=config.ca_file)
        self._context.check_hostname = True
        self._context.verify_mode = ssl.CERT_REQUIRED
        self._context.minimum_version = ssl.TLSVersion.TLSv1_2

    def start(self) -> "LoadRunner":
        self._started = time.monotonic()
        for index in range(self.config.concurrency):
            thread = threading.Thread(
                target=self._worker, name=f"issue34-load-{index}", daemon=True
            )
            thread.start()
            self._threads.append(thread)
        return self

    def _record_error(self, message: str) -> None:
        with self._lock:
            self._counters.errors += 1
            if len(self._error_samples) < MAX_ERROR_SAMPLES:
                self._error_samples.append(message)

    def _reserve_request(self) -> bool:
        if self.config.max_requests <= 0:
            return not self._stop.is_set()
        with self._lock:
            if self._counters.ok + self._counters.errors >= self.config.max_requests:
                self._stop.set()
                return False
            return True

    def _worker(self) -> None:
        connection: _SNIConnection | None = None
        while not self._stop.is_set():
            if not self._reserve_request():
                break
            started = time.monotonic()
            try:
                if connection is None:
                    connection = _SNIConnection(
                        self.config.sni,
                        self.config.port,
                        self._context,
                        self.config.timeout_seconds,
                    )
                headers = {"Host": f"{self.config.sni}:{self.config.port}"}
                connection.request(
                    self.config.method,
                    self.config.path,
                    body=self.config.payload if self.config.method == "POST" else None,
                    headers=headers,
                )
                response = connection.getresponse()
                body = response.read(8 * 1024 * 1024 + 1)
                if len(body) > 8 * 1024 * 1024:
                    raise LoadGenError("response exceeded the bounded read limit")
                if response.status != 200:
                    raise LoadGenError(f"unexpected status {response.status}")
                if self.config.method == "POST" and body != self.config.payload:
                    raise LoadGenError("POST echo digest mismatch")
                if not body:
                    raise LoadGenError("empty response body")
                latency = time.monotonic() - started
                with self._lock:
                    self._counters.ok += 1
                    self._counters.bytes += len(body)
                    self._reservoir.append(latency)
            except (OSError, ssl.SSLError, http.client.HTTPException, LoadGenError) as exc:
                self._record_error(f"{type(exc).__name__}: {exc}")
                if connection is not None:
                    try:
                        connection.close()
                    except OSError:
                        pass
                    connection = None
                time.sleep(0.01)
        if connection is not None:
            try:
                connection.close()
            except OSError:
                pass

    def stop(self) -> None:
        self._stop.set()
        self._finished = time.monotonic()

    def join(self, timeout: float | None = None) -> None:
        deadline = None if timeout is None else time.monotonic() + timeout
        for thread in self._threads:
            remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
            thread.join(timeout=remaining)

    def summary(self) -> dict:
        with self._lock:
            ok = self._counters.ok
            errors = self._counters.errors
            total_bytes = self._counters.bytes
            latencies = sorted(self._reservoir)
            error_samples = list(self._error_samples)
        elapsed = (self._finished or time.monotonic()) - self._started
        elapsed = max(elapsed, 1e-9)

        def percentile(p: float) -> float:
            if not latencies:
                return 0.0
            index = min(len(latencies) - 1, max(0, int(round((len(latencies) - 1) * p))))
            return latencies[index]

        return {
            "requests_ok": ok,
            "errors": errors,
            "bytes": total_bytes,
            "elapsed_seconds": elapsed,
            "requests_per_second": ok / elapsed,
            "latency_p50_ms": percentile(0.50) * 1000.0,
            "latency_p95_ms": percentile(0.95) * 1000.0,
            "latency_p99_ms": percentile(0.99) * 1000.0,
            "latency_samples": len(latencies),
            "error_samples": error_samples,
            "concurrency": self.config.concurrency,
            "method": self.config.method,
        }
