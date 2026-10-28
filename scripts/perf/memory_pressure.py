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
# Cache-fill objects are larger so a small number of admitted L1 entries
# covers a meaningful share of the governor cache budget. The production
# cache still stores and promotes them through the same GET path.
CACHE_PAYLOAD = b"c" * (1024 * 1024)
ORIGIN_CONNECTIONS = set()


def percentile(xs, p):
    return sorted(xs)[max(0, math.ceil(len(xs) * p) - 1)] if xs else None


def event_counters(text):
    return {key: int(value) for key, value in (line.split() for line in text.splitlines())}


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
            bounded = path.startswith(b"/bounded")
            cacheable = path.startswith(b"/cache/")
            body = CACHE_PAYLOAD if cacheable else PAYLOAD
            count = 24000 if slow else (80 if bounded else 1)
            writer.write(f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)*count}\r\nContent-Type: application/octet-stream\r\n\r\n".encode())
            for _ in range(count):
                writer.write(body)
                await writer.drain()
                if slow or bounded:
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
        self.expected = None
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
            s.expected = length
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
                        header_map = dict(e.headers)
                        if b"content-length" in header_map:
                            s.expected = int(header_map[b"content-length"])
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
        index = self.next % len(self.clients)
        self.next += 1
        client = self.clients[index]
        if client.connection_error:
            # H2 idle timeout is 3s under Critical, so a probe pool that is
            # quiet between stages must reconnect rather than report the
            # previous connection's close as a fresh-request refusal.
            try:
                await client.close()
            except Exception:
                pass
            try:
                client = await H2().open()
                self.clients[index] = client
            except Exception as error:
                sample = Sample()
                sample.finish(str(error))
                sample.headers.set()
                return sample
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
                header_map = dict(e.headers)
                if b"content-length" in header_map:
                    s.expected = int(header_map[b"content-length"])
                s.status = int(header_map[b":status"])
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


def progress(sample, before_bytes):
    expected = sample.expected
    ratio = sample.bytes / expected if expected else None
    return {
        "status": sample.status,
        "alive": not sample.done.is_set(),
        "done": sample.done.is_set(),
        "bytes": sample.bytes,
        "bytes_delta": sample.bytes - before_bytes,
        "expected_bytes": expected,
        "progress_ratio": ratio,
        "gap_p99_ms": percentile(sample.gaps_ms, .99),
        "latency_ms": sample.latency_ms,
        "error": sample.error,
        "completed_normally": sample.done.is_set() and sample.error is None and sample.status == 200 and (expected is None or sample.bytes == expected),
    }


def physical_summary(physical):
    events = event_counters(physical.get("memory.events", ""))
    smaps = {**(physical.get("status") or {}), **(physical.get("smaps_rollup") or {})}
    def kib_value(name):
        value = smaps.get(name)
        if not value:
            return None
        try:
            return int(value.split()[0]) * 1024
        except (ValueError, IndexError):
            return None
    return {
        "current_bytes": int(physical["memory.current"]),
        "peak_bytes": int(physical["memory.peak"]),
        "events": events,
        "high_events": events.get("high", 0),
        "max_events": events.get("max", 0),
        "oom_events": events.get("oom", 0),
        "oom_kill_events": events.get("oom_kill", 0),
        "oom_group_kill_events": events.get("oom_group_kill", 0),
        "psi_memory": physical.get("memory.pressure"),
        "host_psi_memory": physical.get("host_memory_pressure"),
        "rss_bytes": kib_value("Rss") or kib_value("VmRSS"),
        "vm_hwm_bytes": kib_value("VmHWM"),
        "pss_bytes": kib_value("Pss"),
        "private_dirty_bytes": kib_value("Private_Dirty"),
        "anonymous_bytes": kib_value("Anonymous"),
    }


SHED_REQUESTED_FIELDS = [
    "final_resort_passes_total",
    "drained_connections_total",
    "critical_samples",
    "reclaim_exhausted",
    "last_final_resort_at_ms",
]


def shed_snapshot(state):
    shed = state.get("shed") or {}
    snapshot = {name: shed.get(name) for name in SHED_REQUESTED_FIELDS}
    snapshot.update(shed)
    snapshot.setdefault("drain_reason", "MemoryPressureShed")
    return snapshot


def rejection_snapshot(state):
    return {
        "h2_stream": state.get("h2Rejects"),
        "h3_request": state.get("h3Rejects"),
        "origin_connect": state.get("originRejects"),
        "request_workspace_used_bytes": (state.get("governor") or {}).get("request_workspace_used_bytes"),
    }


async def fill_cache_batch(client, tag, first, count):
    """Populate production L1 through ordinary GETs.

    The first request for a key stores the object in the disk-backed cache;
    the L1 memory copy is admitted when that object is served from disk, so
    each key is requested twice. Both rounds must succeed for a key to count.
    """
    paths = [f"/cache/{tag}/{index}" for index in range(first, first + count)]

    async def issue(paths):
        samples = [await client.request(path) for path in paths]
        try:
            await asyncio.wait_for(asyncio.gather(*(sample.done.wait() for sample in samples)), 30)
        except asyncio.TimeoutError:
            for sample in samples:
                sample.finish("cache-fill-timeout")
                sample.headers.set()
        return samples

    samples = await issue(paths) + await issue(paths)
    digest = hashlib.sha256(CACHE_PAYLOAD).hexdigest()
    good = [sample for sample in samples if sample.status == 200 and not sample.error and sample.bytes == len(CACHE_PAYLOAD) and sample.digest.hexdigest() == digest]
    return {
        "keys": count,
        "attempted": len(samples),
        "success": len(good),
        "errors": [sample.error or f"status:{sample.status},bytes:{sample.bytes}" for sample in samples if sample not in good][:20],
    }


async def cache_preflight(client, cg):
    before = (await control("stats"))["cache"]["memoryBytes"]
    batch = await fill_cache_batch(client, "preflight", 0, 64)
    after = before
    for _ in range(20):
        state = await control("stats")
        after = state["cache"]["memoryBytes"]
        if after > before:
            break
        await asyncio.sleep(0.2)
    if batch["success"] == 0 or after <= before:
        raise RuntimeError(f"production L1 cache retained no owned bytes: before={before} after={after} batch={batch}")
    return {"before_bytes": before, "after_bytes": after, "memory_current": int((cg / "memory.current").read_text()), **batch}


async def fill_cache(client, cg, tag, max_requests, target_current=None, target_cache=None):
    """Fill the production TinyUFO L1 with ordinary GET responses.

    The stop condition is either the cgroup `memory.current` target or the
    cache's own weight-based owned bytes. No synthetic ballast is marked as
    cache: every byte counted here was admitted and stored by production code.
    """
    state = await control("stats")
    cache_bytes = state["cache"]["memoryBytes"]
    keys = attempted = success = 0
    errors = []
    stalled = 0
    index = 0
    while keys < max_requests:
        current = int((cg / "memory.current").read_text())
        if target_current is not None and current >= target_current:
            break
        if target_cache is not None and cache_bytes >= target_cache:
            break
        batch = await fill_cache_batch(client, tag, index, min(32, max_requests - keys))
        index += batch["keys"]
        keys += batch["keys"]
        attempted += batch["attempted"]
        success += batch["success"]
        errors.extend(batch["errors"])
        if keys % 128 < 32 or keys <= 32 or keys >= max_requests:
            state = await control("stats")
            new_cache_bytes = state["cache"]["memoryBytes"]
            if new_cache_bytes <= cache_bytes:
                stalled += 1
            else:
                stalled = 0
            cache_bytes = new_cache_bytes
            if stalled >= 3:
                break
    state = await control("stats")
    return {
        "keys": keys,
        "attempted": attempted,
        "success": success,
        "errors": errors[:20],
        "cache_bytes": state["cache"]["memoryBytes"],
        "cache_count": state["cache"]["memoryCount"],
        "memory_current": int((cg / "memory.current").read_text()),
        "plateaued": stalled >= 3,
    }


async def cache_component_target():
    budget = int((await control("stats"))["governor"]["cache_budget_bytes"])
    if budget <= 0:
        return 0
    return min(budget, max(8 << 20, budget * 3 // 4))


async def apply_acceptance_pressure(args, source, pct, cg, fill):
    maximum = int((cg / "memory.max").read_text())
    target = maximum * pct // 100
    if source == "cache":
        target_cache = await cache_component_target()
        limit = min(8000, target_cache // len(CACHE_PAYLOAD) + 1024)
        result = await fill_cache(fill, cg, f"acceptance-{args.label}-{pct}", limit, target_cache=target_cache)
        result["kind"] = "cache"
        result["target_bytes"] = target
        result["target_cache_bytes"] = target_cache
        return result
    if source == "ballast":
        state = await control(f"fill {pct}")
        return {"kind": "ballast", "target_bytes": target, "state": state}
    state = await control(f"fill {pct}")
    return {"kind": "mixed", "target_bytes": target, "state": state}


async def normal_completion():
    results = {}
    for proto in ["h1", "h2", "h3"]:
        client = None
        try:
            client = H1() if proto == "h1" else await (H2().open() if proto == "h2" else H3().open())
            row = await burst(client, 3, 2)
            sample = await client.request("/bounded")
            try:
                await asyncio.wait_for(sample.done.wait(), 20)
            except asyncio.TimeoutError:
                sample.finish("timeout")
                sample.headers.set()
            row["bounded"] = progress(sample, 0)
            results[proto] = row
        except Exception as error:
            results[proto] = {"error": repr(error)}
        finally:
            if client is not None:
                await client.close()
    return results


async def probe_bursts(count):
    """Measure new work on freshly established connections.

    Idle H1/H2 connections are closed by their pressure-scaled idle timeouts,
    so reusing a long-lived probe client would report the previous stage's
    idle close as a new-request failure. Each probe set opens its own
    connections and releases them afterwards.
    """
    clients = {"h1": H1(), "h2": await H2Pool().open(4), "h3": await H3().open()}
    try:
        rows = {}
        for proto, client in clients.items():
            rows[proto] = await burst(client, count, 4)
        return rows
    finally:
        for client in clients.values():
            try:
                await client.close()
            except Exception:
                pass


async def recovery_checkpoint(held_samples, cg, pid):
    """Release the pressure source and prove accepted streams resume.

    This checkpoint runs before the extreme final-resort stage, so a kill
    there cannot be mistaken for a recovery failure.
    """
    before = {proto: sample.bytes for proto, sample in held_samples.items()}
    started = time.monotonic()
    await control("release")
    state = await control("stats")
    while state["pressure"] == "critical" and time.monotonic() - started < 30:
        await asyncio.sleep(2)
        state = await control("stats")
    await asyncio.sleep(3)
    state = await control("stats")
    row = {
        "release_seconds": time.monotonic() - started,
        "pressure_below_critical": state["pressure"] != "critical",
        "state": state,
        "physical": observation(cg, pid),
        "new_requests": await probe_bursts(16),
    }
    row["accepted_progress"] = {proto: progress(sample, before[proto]) for proto, sample in held_samples.items()}
    row["shed"] = shed_snapshot(state)
    row["cache"] = state.get("cache")
    row["reclaim"] = state.get("reclaim")
    row["admission_rejects"] = rejection_snapshot(state)
    row["physical_summary"] = physical_summary(row["physical"])
    return row


async def run_acceptance(args, clients, output, cg, pid):
    source = args.acceptance_source
    output["acceptance_source"] = source
    output["source_definition"] = {
        "cache": "production TinyUFO L1 filled and reclaimed through real GET responses; no ballast",
        "ballast": "touched anonymous ballast in the fixture process; no cache fill",
        "mixed": "production TinyUFO L1 plus touched anonymous ballast plus independent burst traffic",
    }[source]
    held = {}
    held_samples = {}
    fill = H1()
    stages = []
    # H2 closes a connection after 200 streams in its churn window. New
    # request probes use several independent connections, distinct from the
    # held long-response connection.
    await clients["h2"].close()
    clients["h2"] = await H2Pool().open(4)
    try:
        for proto in ["h1", "h2", "h3"]:
            held[proto] = H1() if proto == "h1" else await (H2().open() if proto == "h2" else H3().open())
            sample = await held[proto].request("/stream")
            held_samples[proto] = sample
            await asyncio.wait_for(sample.headers.wait(), 10)
            if sample.status != 200 or sample.error:
                raise RuntimeError(f"accepted {proto} long response not admitted: status={sample.status} error={sample.error}")
        if source in ("cache", "mixed"):
            output["preflight"] = await cache_preflight(fill, cg)
        cache_component = None
        if source == "mixed":
            maximum = int((cg / "memory.max").read_text())
            # Keep the cache component above the High-pressure reclaim target
            # (`cache_budget / 2`) so the run can observe real L1 eviction.
            cache_component = min(await cache_component_target(), maximum // 2)
            if cache_component > 0:
                limit = min(8000, cache_component // len(CACHE_PAYLOAD) + 1024)
                output["cache_fill"] = await fill_cache(fill, cg, f"acceptance-{args.label}", limit, target_cache=cache_component)
        output["stages"] = stages
        initial_shed = (await control("stats")).get("shed") or {}
        initial_passes = initial_shed.get("final_resort_passes_total") or 0
        initial_drained = initial_shed.get("drained_connections_total") or 0
        last_shed = initial_shed
        for pct in [50, 70, 85, 90, 94, 97]:
            if (last_shed.get("final_resort_passes_total") or 0) > initial_passes:
                output["stopped_after_final_resort_at"] = output["stages"][-1]["target_percent"]
                break
            before_bytes = {proto: sample.bytes for proto, sample in held_samples.items()}
            try:
                applied = await apply_acceptance_pressure(args, source, pct, cg, fill)
            except asyncio.TimeoutError:
                output["interrupted_stage"] = {"target_percent": pct, "error": "control timeout"}
                break
            maximum = int((cg / "memory.max").read_text())
            current = int((cg / "memory.current").read_text())
            target = maximum * pct // 100
            reached = current >= (target if source == "cache" else target - (1 << 20))
            dwell = 6 if reached else 2
            await asyncio.sleep(dwell)
            state = await control("stats")
            row = {
                "target_percent": pct,
                "source": source,
                "reached": reached,
                "dwell_seconds": dwell,
                "applied": applied,
                "state": state,
                "physical": observation(cg, pid),
                "new_requests": {},
            }
            row["physical_summary"] = physical_summary(row["physical"])
            row["cache"] = state.get("cache")
            row["reclaim"] = state.get("reclaim")
            row["shed"] = shed_snapshot(state)
            row["admission_rejects"] = rejection_snapshot(state)
            row["new_requests"] = await probe_bursts(32)
            row["accepted_progress"] = {proto: progress(sample, before_bytes[proto]) for proto, sample in held_samples.items()}
            row["isolation"] = {
                proto: {
                    "held_alive": not sample.done.is_set(),
                    "held_progress": sample.bytes - before_bytes[proto],
                    "independent_new_requests": row["new_requests"].get(proto, {}).get("success"),
                }
                for proto, sample in held_samples.items()
            }
            stages.append(row)
            last_shed = row["shed"]
            print(json.dumps(row), flush=True)
            if pct in (85, 90) and "mid_recovery" not in output:
                alive = all(not sample.done.is_set() for sample in held_samples.values())
                passes = row["shed"].get("final_resort_passes_total") or 0
                if alive and passes == initial_passes:
                    output["mid_recovery"] = await recovery_checkpoint(
                        held_samples, cg, pid
                    )
                    print(json.dumps(output["mid_recovery"]), flush=True)
                else:
                    output["mid_recovery"] = {
                        "skipped": True,
                        "held_alive": alive,
                        "final_resort_passes_total": passes,
                        "reason": "accepted work already ended before the recovery checkpoint",
                    }
        # Extreme last resort: hold at the 98% safety ceiling long enough for
        # the sustained-Critical gate (10s, samples 5s apart), fresh
        # ineffective-reclaim evidence, and the 30s final-resort rate limit.
        before_bytes = {proto: sample.bytes for proto, sample in held_samples.items()}
        before_passes = initial_passes
        before_drained = initial_drained
        final_seen = (last_shed.get("final_resort_passes_total") or 0) > initial_passes
        if final_seen:
            # The previous stage already reached the hard-exhaustion fallback;
            # pushing further only stalls the fixture's allocator under a full
            # cgroup and repeats the same evidence.
            applied = {
                "kind": "skipped_after_final_resort",
                "trigger_stage": stages[-1]["target_percent"],
            }
            reached = stages[-1]["reached"]
            dwell = 0
            state = stages[-1]["state"]
        else:
            try:
                applied = await apply_acceptance_pressure(args, source, 98, cg, fill)
            except asyncio.TimeoutError:
                output["final_control_timeout"] = True
                applied = {"kind": "control_timeout"}
                reached = False
                dwell = 0
                state = stages[-1]["state"] if stages else await control("stats")
            else:
                maximum = int((cg / "memory.max").read_text())
                current = int((cg / "memory.current").read_text())
                target = maximum * 98 // 100
                reached = current >= (target if source == "cache" else target - (1 << 20))
                dwell = 30 if reached else 2
                await asyncio.sleep(dwell)
                state = await control("stats")
        after_shed = state.get("shed") or {}
        after_passes = after_shed.get("final_resort_passes_total") or 0
        after_drained = after_shed.get("drained_connections_total") or 0
        row = {
            "target_percent": stages[-1]["target_percent"] if final_seen else 98,
            "requested_percent": 98,
            "source": source,
            "reached": reached,
            "dwell_seconds": dwell,
            "applied": applied,
            "state": state,
            "physical": observation(cg, pid),
            "new_requests": {},
            "last_resort": {
                "reached": reached,
                "trigger_stage": stages[-1]["target_percent"] if final_seen else 98,
                "final_resort_passes_delta": after_passes - before_passes,
                "final_resort_passes_total": after_shed.get("final_resort_passes_total"),
                "final_resort_seen_in_run": after_passes > before_passes,
                "passes_before_run": before_passes,
                "last_final_resort_at_ms": after_shed.get("last_final_resort_at_ms"),
                "reclaim_exhausted": after_shed.get("reclaim_exhausted"),
                "critical_samples": after_shed.get("critical_samples"),
                "drained_connections_delta": after_drained - before_drained,
                "drained_connections_total": after_shed.get("drained_connections_total"),
                "drained_before_run": before_drained,
                "drain_reason": after_shed.get("drain_reason", "MemoryPressureShed"),
                "unavailable_fields": [name for name in SHED_REQUESTED_FIELDS if name not in after_shed],
            },
        }
        row["physical_summary"] = physical_summary(row["physical"])
        row["cache"] = state.get("cache")
        row["reclaim"] = state.get("reclaim")
        row["shed"] = shed_snapshot(state)
        row["admission_rejects"] = rejection_snapshot(state)
        row["new_requests"] = await probe_bursts(16)
        row["accepted_progress"] = {proto: progress(sample, before_bytes[proto]) for proto, sample in held_samples.items()}
        if not final_seen:
            stages.append(row)
        else:
            output["post_final"] = row
        print(json.dumps(row), flush=True)
        output["stages"] = stages
        output["last_resort"] = row["last_resort"]
        if source == "cache":
            # Cache alone is bounded below the High threshold by the governor,
            # so a pressure-triggered reclaim cannot occur. Prove the retained
            # bytes are the real production L1 by invoking the same production
            # reclaim path directly and recording its own stats.
            before_reclaim = await control("stats")
            requested = False
            attempts = 0
            after_reclaim = before_reclaim
            for _ in range(3):
                attempts += 1
                reply = await control("reclaim critical")
                requested = requested or bool(reply.get("forcedReclaim"))
                await asyncio.sleep(0.5)
                after_reclaim = await control("stats")
                if after_reclaim["cache"]["memoryBytes"] < before_reclaim["cache"]["memoryBytes"]:
                    break
                await asyncio.sleep(6)
            output["cache_reclaim"] = {
                "requested": requested,
                "attempts": attempts,
                "before_cache_bytes": before_reclaim["cache"]["memoryBytes"],
                "after_cache_bytes": after_reclaim["cache"]["memoryBytes"],
                "l1_bytes_delta": after_reclaim["cache"]["memoryBytes"] - before_reclaim["cache"]["memoryBytes"],
                "before_reclaim": before_reclaim.get("reclaim"),
                "after_reclaim": after_reclaim.get("reclaim"),
            }
        await control("release")
        output["recovery"] = []
        for index in range(30):
            await asyncio.sleep(3)
            state = await control("stats")
            recovery = {
                "elapsed_seconds": (index + 1) * 3,
                "state": state,
                "physical": observation(cg, pid),
                "cache": state.get("cache"),
                "reclaim": state.get("reclaim"),
                "shed": shed_snapshot(state),
                "admission_rejects": rejection_snapshot(state),
                "accepted_progress": {proto: progress(sample, 0) for proto, sample in held_samples.items()},
            }
            recovery["physical_summary"] = physical_summary(recovery["physical"])
            output["recovery"].append(recovery)
            print(json.dumps(recovery), flush=True)
            if state["pressure"] == "normal" and index >= 2:
                break
        output["held_completion"] = {proto: progress(sample, 0) for proto, sample in held_samples.items()}
        output["normal_completion"] = await normal_completion()
        output["shed_field_support"] = {
            "available": [name for name in SHED_REQUESTED_FIELDS if name in (state.get("shed") or {})],
            "missing_on_this_build": [name for name in SHED_REQUESTED_FIELDS if name not in (state.get("shed") or {})],
        }
    finally:
        for client in held.values():
            try:
                await client.close()
            except Exception:
                pass
        try:
            await fill.close()
        except Exception:
            pass
        try:
            await control("release")
        except Exception:
            pass


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
        if args.acceptance_source:
            await run_acceptance(args, clients, output, cg, pid)
        elif args.abuse:
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
            try:
                await client.close()
            except Exception:
                pass
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
    p.add_argument("--acceptance-source", choices=["cache", "ballast", "mixed"])
    p.add_argument("--count", action="store_true")
    p.add_argument("--requests", type=int, default=4000)
    p.add_argument("--concurrency", type=int, default=16)
    p.add_argument("--label", default="candidate")
    p.add_argument("--output", default="pressure-result.json")
    asyncio.run(main(p.parse_args()))
