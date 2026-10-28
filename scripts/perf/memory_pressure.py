#!/usr/bin/env python3
"""Drive the loopback production EdgeProxy fixture; keep clients outside its cgroup.
Requires h2 and aioquic. Writes JSON with raw per-stage samples, not just averages.
Run with --origin-only to host the deterministic origin for paired benchmarks.
"""
import argparse
import asyncio
import hashlib
import json
import math
import pathlib
import ssl
import time

import h2.config
import h2.connection
import h2.events
from aioquic.asyncio import connect, QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ConnectionTerminated, StreamReset

PAYLOAD = b"p" * 16384
ORIGIN_CONNECTIONS = set()


def percentile(xs, p):
    return sorted(xs)[max(0, math.ceil(len(xs) * p) - 1)] if xs else None


async def control(command):
    reader, writer = await asyncio.open_connection("127.0.0.1", 19090)
    writer.write((command + "\n").encode())
    await writer.drain()
    reply = await asyncio.wait_for(reader.readline(), 60)
    writer.close()
    await writer.wait_closed()
    return json.loads(reply)


async def origin(reader, writer):
    ORIGIN_CONNECTIONS.add(writer)
    try:
        while True:
            headers = await reader.readuntil(b"\r\n\r\n")
            path = headers.split(b" ")[1]
            fields = dict(line.split(b":", 1) for line in headers.split(b"\r\n")[1:] if b":" in line)
            fields = {key.lower(): value.strip().lower() for key, value in fields.items()}
            if fields.get(b"transfer-encoding") == b"chunked":
                while True:
                    size = int((await reader.readline()).split(b";", 1)[0], 16)
                    if size == 0:
                        while True:
                            trailer = await reader.readline()
                            if not trailer:
                                raise ConnectionError("incomplete chunked trailers")
                            if trailer == b"\r\n":
                                break
                        break
                    await reader.readexactly(size + 2)
            elif b"content-length" in fields:
                await reader.readexactly(int(fields[b"content-length"]))
            # Slow requests emit data for the entire ramp. Headers and the
            # first chunk prove acceptance before the first pressure stage.
            slow = path.startswith(b"/stream") or path.startswith(b"/hold")
            count = 24000 if slow else 1
            writer.write(f"HTTP/1.1 200 OK\r\nContent-Length: {len(PAYLOAD)*count}\r\nContent-Type: application/octet-stream\r\n\r\n".encode())
            for _ in range(count):
                writer.write(PAYLOAD)
                await writer.drain()
                if slow:
                    await asyncio.sleep(0.025)
    except (asyncio.IncompleteReadError, ConnectionError, asyncio.CancelledError):
        pass
    finally:
        writer.close()


class Sample:
    def __init__(self):
        self.started = time.perf_counter()
        self.headers = asyncio.Event()
        self.done = asyncio.Event()
        self.status = None
        self.bytes = 0
        self.error = None
        self.latency_ms = None
        self.last_data = None
        self.gaps_ms = []
        self.digest = hashlib.sha256()

    def data(self, data):
        now = time.perf_counter()
        if self.last_data is not None:
            self.gaps_ms.append((now - self.last_data) * 1000)
        self.last_data = now
        self.bytes += len(data)
        self.digest.update(data)

    def finish(self, error=None):
        if self.done.is_set():
            return
        self.error = error
        self.latency_ms = (time.perf_counter() - self.started) * 1000
        self.done.set()


class H1:
    def __init__(self):
        self.samples = []
        self.tasks = []
        self.idle = []

    async def request(self, path):
        s = Sample()
        self.samples.append(s)
        self.tasks.append(asyncio.create_task(self._request(s, path)))
        return s

    async def _request(self, s, path):
        writer = None
        try:
            if self.idle:
                reader, writer = self.idle.pop()
            else:
                reader, writer = await asyncio.open_connection("127.0.0.1", 19080)
            writer.write(f"GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n".encode())
            await writer.drain()
            headers = await reader.readuntil(b"\r\n\r\n")
            s.status = int(headers.split(b" ")[1])
            length = next(int(x.split(b":", 1)[1]) for x in headers.split(b"\r\n") if x.lower().startswith(b"content-length:"))
            s.headers.set()
            while s.bytes < length:
                data = await reader.read(min(16384, length - s.bytes))
                if not data:
                    raise ConnectionError("short body")
                s.data(data)
            s.finish()
            if b"connection: close" not in headers.lower():
                self.idle.append((reader, writer))
                writer = None
        except Exception as e:
            s.finish(str(e))
            s.headers.set()
        finally:
            if writer:
                writer.close()

    async def close(self):
        for _, writer in self.idle:
            writer.close()
        for task in self.tasks:
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)


class H2:
    async def open(self):
        ctx = ssl._create_unverified_context()
        ctx.set_alpn_protocols(["h2"])
        self.reader, self.writer = await asyncio.open_connection("127.0.0.1", 19443, ssl=ctx, server_hostname="localhost")
        assert self.writer.get_extra_info("ssl_object").selected_alpn_protocol() == "h2"
        self.h = h2.connection.H2Connection(config=h2.config.H2Configuration(client_side=True))
        self.samples = {}
        self.connection_error = None
        self.settings = asyncio.Event()
        self.h.initiate_connection()
        self.writer.write(self.h.data_to_send())
        await self.writer.drain()
        self.task = asyncio.create_task(self.receive())
        # Wait for the server's actual SETTINGS before the abusive-stream case.
        await asyncio.wait_for(self.settings.wait(), 5)
        return self

    async def request(self, path):
        s = Sample()
        if self.connection_error:
            s.finish(self.connection_error)
            s.headers.set()
            return s
        try:
            stream = self.h.get_next_available_stream_id()
            self.h.send_headers(stream, [(":method", "GET"), (":scheme", "https"), (":authority", "localhost"), (":path", path)], end_stream=True)
            self.samples[stream] = s
            self.writer.write(self.h.data_to_send())
            await self.writer.drain()
        except Exception as e:
            s.finish(str(e))
            s.headers.set()
        return s

    async def receive(self):
        try:
            while data := await self.reader.read(65536):
                for e in self.h.receive_data(data):
                    if isinstance(e, h2.events.RemoteSettingsChanged):
                        self.settings.set()
                    s = self.samples.get(getattr(e, "stream_id", None))
                    if isinstance(e, h2.events.DataReceived):
                        self.h.acknowledge_received_data(e.flow_controlled_length, e.stream_id)
                        if s:
                            s.data(e.data)
                    if s and isinstance(e, h2.events.ResponseReceived):
                        s.status = int(dict(e.headers)[b":status"])
                        s.headers.set()
                    if s and isinstance(e, h2.events.StreamEnded):
                        s.finish()
                    if s and isinstance(e, h2.events.StreamReset):
                        s.finish(f"reset:{e.error_code}")
                        s.headers.set()
                self.writer.write(self.h.data_to_send())
                await self.writer.drain()
            raise ConnectionError("h2 connection closed")
        except Exception as e:
            self.connection_error = str(e)
            for s in self.samples.values():
                if not s.done.is_set():
                    s.finish(str(e))
                    s.headers.set()

    async def close(self):
        self.task.cancel()
        self.writer.close()
        await asyncio.gather(self.task, return_exceptions=True)
        await self.writer.wait_closed()


class H2Pool:
    async def open(self, size):
        self.clients = [await H2().open() for _ in range(size)]
        self.next = 0
        return self

    async def request(self, path):
        client = self.clients[self.next % len(self.clients)]
        self.next += 1
        return await client.request(path)

    async def close(self):
        await asyncio.gather(*(client.close() for client in self.clients))


class H3Protocol(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.http = H3Connection(self._quic)
        self.samples = {}
        self.connection_error = None

    def quic_event_received(self, event):
        if isinstance(event, ConnectionTerminated):
            self.connection_error = f"ConnectionTerminated:{event.error_code}"
        if isinstance(event, (ConnectionTerminated, StreamReset)):
            for stream, sample in self.samples.items():
                if not sample.done.is_set() and (isinstance(event, ConnectionTerminated) or stream == event.stream_id):
                    sample.finish(f"{type(event).__name__}:{event.error_code}")
                    sample.headers.set()
        for e in self.http.handle_event(event):
            s = self.samples.get(e.stream_id)
            if s is None:
                continue
            if isinstance(e, HeadersReceived):
                s.status = int(dict(e.headers)[b":status"])
                s.headers.set()
            if isinstance(e, DataReceived):
                s.data(e.data)
            if e.stream_ended:
                s.finish()

    async def request(self, path):
        s = Sample()
        if self.connection_error:
            s.finish(self.connection_error)
            s.headers.set()
            return s
        stream = self._quic.get_next_available_stream_id()
        self.samples[stream] = s
        self.http.send_headers(stream, [(b":method", b"GET"), (b":scheme", b"https"), (b":authority", b"localhost"), (b":path", path.encode())], end_stream=True)
        self.transmit()
        return s


class H3:
    async def open(self):
        conf = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, verify_mode=ssl.CERT_NONE)
        self.context = connect("127.0.0.1", 19443, configuration=conf, create_protocol=H3Protocol)
        self.client = await self.context.__aenter__()
        return self

    async def request(self, path):
        return await self.client.request(path)

    async def close(self):
        await self.context.__aexit__(None, None, None)


def observation(cg, pid):
    result = {}
    for name in ["memory.current", "memory.peak", "memory.events", "memory.pressure", "memory.stat", "cpu.stat"]:
        result[name] = (cg / name).read_text().strip()
    result["host_memory_pressure"] = pathlib.Path("/proc/pressure/memory").read_text().strip()
    for name in ["status", "smaps_rollup"]:
        try:
            text = pathlib.Path(f"/proc/{pid}/{name}").read_text()
            result[name] = {x.split(":")[0]: x.split(":", 1)[1].strip() for x in text.splitlines() if ":" in x and x.split(":")[0] in ["VmRSS", "VmHWM", "Rss", "Pss", "Private_Dirty", "Anonymous"]}
        except FileNotFoundError:
            result[name] = {"error": "process exited"}
    return result


async def burst(client, n, concurrency):
    samples = []
    async def worker(count):
        for _ in range(count):
            s = await client.request("/fast")
            try:
                await asyncio.wait_for(s.done.wait(), 5)
            except asyncio.TimeoutError:
                s.finish("timeout")
            samples.append(s)
            # One terminal failure per worker is enough to establish that this
            # pressure-stage probe failed; keep unattempted requests explicit.
            if s.error:
                break
    started = time.perf_counter()
    await asyncio.gather(*(worker(n // concurrency + (i < n % concurrency)) for i in range(concurrency)))
    elapsed = time.perf_counter() - started
    good = [s for s in samples if s.status == 200 and not s.error and s.bytes == len(PAYLOAD) and s.digest.hexdigest() == hashlib.sha256(PAYLOAD).hexdigest()]
    xs = [s.latency_ms for s in good]
    return {"requests": n, "attempted": len(samples), "unattempted_after_failure": n - len(samples), "success": len(good), "elapsed": elapsed, "rps": len(good) / elapsed, "p50_ms": percentile(xs, .5), "p99_ms": percentile(xs, .99), "latencies_ms": xs, "errors": [s.error or f"status:{s.status},bytes:{s.bytes}" for s in samples if s not in good][:20]}


async def main(args):
    server = await asyncio.start_server(origin, "127.0.0.1", 19081)
    if args.origin_only:
        async with server:
            await server.serve_forever()
        return
    output = {"label": args.label, "stages": [], "benchmark": []}
    clients = {}
    try:
        clients = {"h1": H1(), "h2": await H2().open(), "h3": await H3().open()}
        first = await control("stats")
        cg = pathlib.Path(first["cgroup"])
        pid = first["pid"]
        if args.abuse:
            victim = {"h2": await H2().open(), "h3": await H3().open()}
            try:
                held = {}
                for proto, client in victim.items():
                    s = await client.request("/stream")
                    await asyncio.wait_for(s.headers.wait(), 10)
                    assert s.status == 200 and not s.error
                    held[proto] = s
                output["abuse"] = []
                # Bounded local test. Concurrency exceeds advertised limits;
                # local H2 refusals and remote stream rejection are recorded.
                for proto in ["h2", "h3"]:
                    attacker = clients[proto]
                    samples = []
                    for _ in range(768):
                        samples.append(await attacker.request("/hold"))
                    await asyncio.sleep(3)
                    state = await control("stats")
                    before = {p: s.bytes for p, s in held.items()}
                    probe = {p: await burst(c, 30, 2) for p, c in victim.items()}
                    await asyncio.sleep(1)
                    row = {"protocol": proto, "attempts": len(samples), "accepted": sum(s.status == 200 for s in samples), "local_or_remote_errors": sum(bool(s.error) for s in samples), "pending": sum(not s.headers.is_set() for s in samples), "state": state, "victim_new_requests": probe, "victim_continuation": {p: {"bytes_delta": s.bytes-before[p], "error": s.error, "alive": not s.done.is_set()} for p,s in held.items()}, "physical": observation(cg,pid)}
                    output["abuse"].append(row)
                    print(json.dumps(row), flush=True)
                    await attacker.close()
                    clients.pop(proto)
            finally:
                for client in victim.values():
                    await client.close()
        elif args.benchmark:
            # Production closes H2 after 200 streams in a 30s window. Spread
            # this bounded sample across connections below that policy limit.
            await clients["h2"].close()
            clients["h2"] = await H2Pool().open(max(32, math.ceil((args.requests + 200) / 150)))
            for proto, client in clients.items():
                await burst(client, 200, 4)
                before = await control("count on" if args.count else "count off")
                measured = await burst(client, args.requests, args.concurrency)
                after = await control("count off")
                measured.update({"protocol": proto, "allocations": after["allocations"] - before["allocations"], "allocatedBytes": after["allocatedBytes"] - before["allocatedBytes"], "reallocations": after["reallocations"] - before["reallocations"]})
                output["benchmark"].append(measured)
                print(json.dumps(measured), flush=True)
        else:
            held = {}
            for proto, client in clients.items():
                s = await client.request("/stream")
                await asyncio.wait_for(s.headers.wait(), 10)
                assert s.status == 200 and not s.error, (proto, s.error, s.status)
                held[proto] = s
            for pct in [50, 70, 85, 90, 94, 97]:
                before_bytes = {p: s.bytes for p, s in held.items()}
                state = await control(f"fill {pct}")
                await asyncio.sleep(6 if args.continuation_only else 2.5)
                row = {"target_percent": pct, "state": await control("stats"), "physical": observation(cg, pid), "new_requests": {}}
                if not args.continuation_only:
                    for proto, client in clients.items():
                        row["new_requests"][proto] = await burst(client, 50, 4)
                row["accepted_progress"] = {p: {"alive": not s.done.is_set(), "bytes_delta": s.bytes - before_bytes[p], "total_bytes": s.bytes, "gap_p99_ms": percentile(s.gaps_ms, .99), "error": s.error} for p, s in held.items()}
                output["stages"].append(row)
                print(json.dumps(row), flush=True)
            await control("release")
        output["final"] = await control("stats")
    except Exception as e:
        output["failure"] = repr(e)
        raise
    finally:
        pathlib.Path(args.output).write_text(json.dumps(output, indent=2))
        output["cleanup_seconds"] = {}
        for proto, client in clients.items():
            started = time.monotonic()
            await client.close()
            output["cleanup_seconds"][proto] = time.monotonic() - started
            pathlib.Path(args.output).write_text(json.dumps(output, indent=2))
        server.close()
        # Keep-alive origin sockets would otherwise hold the fixture process
        # open after the measured clients have gone.
        for writer in list(ORIGIN_CONNECTIONS):
            writer.close()
        await server.wait_closed()
        pathlib.Path(args.output).write_text(json.dumps(output, indent=2))


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--origin-only", action="store_true")
    p.add_argument("--benchmark", action="store_true")
    p.add_argument("--continuation-only", action="store_true")
    p.add_argument("--abuse", action="store_true")
    p.add_argument("--count", action="store_true")
    p.add_argument("--requests", type=int, default=4000)
    p.add_argument("--concurrency", type=int, default=16)
    p.add_argument("--label", default="candidate")
    p.add_argument("--output", default="pressure-result.json")
    asyncio.run(main(p.parse_args()))
