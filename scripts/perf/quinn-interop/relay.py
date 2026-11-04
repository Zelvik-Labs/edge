"""Bounded IPv4 loopback relay for a diagnostic, never a capacity comparison."""
from collections import deque
import hashlib
import json
import os
from pathlib import Path
import selectors
import socket
import threading
import time
from urllib.parse import urlsplit, urlunsplit


class UdpRelay:
    MAX_FLOWS = 20_000
    MAX_QUEUED = 32_768
    MAX_BYTES = 64 * 1024 * 1024

    def __init__(self, url, interval_us, output, cpu=None):
        parsed = urlsplit(url)
        if parsed.scheme != 'https' or parsed.hostname not in ['localhost', '127.0.0.1'] or not parsed.port:
            raise ValueError('relay only accepts the isolated IPv4 loopback HTTPS fixture')
        if interval_us not in [0, 100]:
            raise ValueError('diagnostic relay interval must be 0 or 100 us')
        self.target = ('127.0.0.1', parsed.port)
        self.output = Path(output)
        self.cpu = cpu
        self.front = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.front.bind(('127.0.0.1', 0))
        self.front.setblocking(False)
        self.url = urlunsplit(parsed._replace(netloc=f'localhost:{self.front.getsockname()[1]}'))
        self.stop = threading.Event()
        self.finished = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self._run, name='quic-ingress-relay', daemon=True)
        self.stats = {'source_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'scope': 'IPv4 loopback UDP relay; server-to-client send spacing, not client internal batch proof or capacity',
            'target_url': url, 'client_url': self.url, 'interval_us': interval_us,
            'flows': 0, 'received_client': 0, 'forwarded_client': 0,
            'received_server': 0, 'forwarded_server': 0, 'peak_queued': 0,
            'peak_bytes': 0, 'queued_at_shutdown': 0, 'bytes_at_shutdown': 0,
            'minimum_server_send_interval_ns': None, 'cpu': cpu, 'affinity': None, 'error': None}

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, kind, value, traceback):
        self.stop.set()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise RuntimeError('diagnostic UDP relay failed to terminate')
        if not self.finished.is_set():
            raise RuntimeError('diagnostic UDP relay did not retain shutdown metadata')
        if self.error is not None:
            raise RuntimeError('diagnostic UDP relay failed; retained relay metadata') from self.error

    def _run(self):
        selector = selectors.DefaultSelector()
        upstream = {}
        pending = deque()
        queued_bytes = 0
        last_send = None
        next_send = 0
        interval = self.stats['interval_us'] * 1000
        started = time.monotonic_ns()
        started_cpu = time.thread_time_ns()
        try:
            if self.cpu is not None:
                os.sched_setaffinity(0, {self.cpu})
                self.stats['affinity'] = sorted(os.sched_getaffinity(0))
                if self.stats['affinity'] != [self.cpu]:
                    raise RuntimeError('relay CPU pinning did not take effect')
            selector.register(self.front, selectors.EVENT_READ, ('client', None))
            while not self.stop.is_set():
                now = time.monotonic_ns()
                sent = 0
                while pending and now >= next_send and sent < 64:
                    data, client = pending.popleft()
                    queued_bytes -= len(data)
                    if self.front.sendto(data, client) != len(data):
                        raise RuntimeError('partial diagnostic downstream datagram')
                    actual = time.monotonic_ns()
                    if last_send is not None:
                        delta = actual - last_send
                        minimum = self.stats['minimum_server_send_interval_ns']
                        self.stats['minimum_server_send_interval_ns'] = delta if minimum is None else min(minimum, delta)
                    last_send = actual
                    next_send = actual + interval
                    self.stats['forwarded_server'] += 1
                    sent += 1
                    now = time.monotonic_ns()
                # epoll rounds short nonzero timeouts to milliseconds. Poll
                # the final 100us on the dedicated fixture core instead of
                # turning the requested spacing into millisecond pacing.
                timeout = 0 if pending else 0.01
                for key, _ in selector.select(timeout):
                    direction, client = key.data
                    for _ in range(64):
                        try:
                            if direction == 'client':
                                data, client = self.front.recvfrom(65536)
                            else:
                                data = key.fileobj.recv(65536)
                        except BlockingIOError:
                            break
                        if direction == 'client':
                            self.stats['received_client'] += 1
                            if client not in upstream:
                                if len(upstream) >= self.MAX_FLOWS:
                                    raise RuntimeError('diagnostic relay flow bound exhausted')
                                channel = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                                channel.connect(self.target)
                                channel.setblocking(False)
                                upstream[client] = channel
                                selector.register(channel, selectors.EVENT_READ, ('server', client))
                                self.stats['flows'] = len(upstream)
                            if upstream[client].send(data) != len(data):
                                raise RuntimeError('partial diagnostic upstream datagram')
                            self.stats['forwarded_client'] += 1
                        else:
                            self.stats['received_server'] += 1
                            if len(pending) >= self.MAX_QUEUED or queued_bytes + len(data) > self.MAX_BYTES:
                                raise RuntimeError('diagnostic relay queue bound exhausted')
                            pending.append((data, client))
                            queued_bytes += len(data)
                            self.stats['peak_queued'] = max(self.stats['peak_queued'], len(pending))
                            self.stats['peak_bytes'] = max(self.stats['peak_bytes'], queued_bytes)
            # No transfer continues after the calling client has exited. Keep
            # outstanding shutdown datagrams visible instead of hiding drops.
        except Exception as error:
            self.error = error
            self.stats['error'] = repr(error)
        finally:
            self.stats['wall_ns'] = time.monotonic_ns() - started
            self.stats['thread_cpu_ns'] = time.thread_time_ns() - started_cpu
            self.stats['queued_at_shutdown'] = len(pending)
            self.stats['bytes_at_shutdown'] = queued_bytes
            for channel in upstream.values():
                channel.close()
            selector.close()
            self.front.close()
            self.output.parent.mkdir(parents=True, exist_ok=True)
            self.output.write_text(json.dumps(self.stats, indent=2) + '\n')
            self.finished.set()
