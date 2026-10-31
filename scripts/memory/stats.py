"""Strict allocator-stats ingestion for the Issue 34 soak target.

At Edge 40fda4f there is no production allocator/cgroup stats transport: the
edge node uploads ``status_json`` over gRPC to the control plane and exposes
no HTTP/file endpoint. The soak therefore requires a purpose-built target (the
Issue 34 harness) that writes ``mi_stats_get_json`` plus ``mi_process_info``.

Missing fields are a hard error. ``0`` is returned only when the field is
present and zero.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request

DEFAULT_REQUIRED_KEYS = (
    "process.commit_current",
    "process.peak_commit",
    "process.rss_current",
    "process.page_faults",
)


class StatsError(RuntimeError):
    """The allocator stats surface is missing, unreadable or incomplete."""


def _lookup(document: dict, dotted: str, *, signed: bool = False):
    node = document
    for part in dotted.split("."):
        if not isinstance(node, dict) or part not in node:
            raise StatsError(f"allocator stats missing required field {dotted!r}")
        node = node[part]
    if isinstance(node, bool) or not isinstance(node, (int, float)):
        raise StatsError(f"allocator stats field {dotted!r} is not numeric: {node!r}")
    if node < 0 and not signed:
        raise StatsError(f"allocator stats field {dotted!r} is negative: {node!r}")
    return node


def normalize(document: dict, required_keys=DEFAULT_REQUIRED_KEYS) -> dict:
    """Validate and flatten a mimalloc ``mi_stats_get_json`` document.

    Accepts the native shape (``process.commit_current``) and a flat harness
    shape (``allocator.commit_current``); the native shape wins.
    """
    if not isinstance(document, dict):
        raise StatsError(f"allocator stats root must be an object, got {type(document).__name__}")
    flat: dict[str, float] = {}
    for key in required_keys:
        try:
            flat[key] = float(_lookup(document, key))
        except StatsError:
            alternate = "allocator." + key.split(".", 1)[1]
            flat[key] = float(_lookup(document, alternate))
    process = document.get("process")
    if isinstance(process, dict):
        for name in ("commit_current", "peak_commit", "rss_current", "peak_rss", "page_faults"):
            if name in process:
                flat[f"process.{name}"] = float(_lookup(document, f"process.{name}"))
    # bench-defense wraps the native v3 document under `mimalloc`. Keep its
    # count objects and bin totals distinct from mi_process_info above.
    native = document.get("mimalloc")
    if isinstance(native, dict):
        for name in (
            "reserved", "committed", "page_committed", "pages",
            "pages_abandoned", "malloc_normal", "malloc_huge", "malloc_requested",
        ):
            if name not in native:
                continue
            for field in ("current", "peak", "total"):
                key = f"mimalloc.{name}.{field}"
                # Native signed counts aggregate thread-local deltas and may
                # be negative. Preserve the raw value; process/RSS stays strict.
                flat[key] = float(_lookup(document, key, signed=True))
        for name in ("reset", "purged", "pages_retire", "pages_reclaim_on_free"):
            if name in native:
                key = f"mimalloc.{name}"
                flat[key] = float(_lookup(document, key))
        for name in ("malloc_bins", "page_bins", "chunk_bins"):
            if name not in native:
                continue
            bins = native[name]
            if not isinstance(bins, list):
                raise StatsError(f"allocator stats mimalloc.{name} is not an array")
            flat[f"mimalloc.{name}.count"] = float(len(bins))
            # Native bins hold block/page counts, not bytes. Do not sum
            # per-bin peaks: they can occur at different instants.
            for field in ("current", "total"):
                total = 0.0
                for item in bins:
                    total += float(_lookup({"bin": item}, f"bin.{field}", signed=True))
                flat[f"mimalloc.{name}.{field}"] = total
    worker = document.get("worker_reclaim")
    if isinstance(worker, dict):
        for name in ("started", "live", "park_collects", "stop_collects"):
            key = f"worker_reclaim.{name}"
            flat[key] = float(_lookup(document, key))
    # Optional allocator cache/residency counters, still strict when present.
    for name in ("reserved", "committed", "reset", "retained", "cached"):
        if name in document:
            flat[name] = float(_lookup(document, name))
    # Bounded allocator-cache summary. The raw mimalloc document can be large;
    # keeping full bin arrays in every 1 s JSONL sample would bloat the log, so
    # retain counts and numeric totals only.
    for name in ("malloc_bins", "page_bins", "chunk_bins"):
        bins = document.get(name)
        if not isinstance(bins, list):
            continue
        flat[f"{name}.count"] = float(len(bins))
        for field in ("cached", "committed", "reserved", "used", "total"):
            values = [
                float(item[field])
                for item in bins
                if isinstance(item, dict)
                and isinstance(item.get(field), (int, float))
                and not isinstance(item.get(field), bool)
            ]
            if values:
                flat[f"{name}.{field}"] = sum(values)
    return flat


def read_file(path: str) -> dict:
    """Read the most recent complete JSON object from ``path``.

    The harness may append one JSON object per line or rewrite a single
    object. Both are accepted; a partially written trailing line is ignored,
    but an empty/unparsable file is an error.
    """
    try:
        with open(path, "r", encoding="utf-8") as handle:
            text = handle.read()
    except OSError as exc:
        raise StatsError(f"cannot read allocator stats file {path}: {exc}") from exc
    candidates = [line for line in text.splitlines() if line.strip()]
    if not candidates:
        raise StatsError(f"allocator stats file {path} is empty")
    for line in reversed(candidates):
        try:
            document = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(document, dict):
            return document
    raise StatsError(f"allocator stats file {path} has no complete JSON object")


def read_url(url: str, timeout: float = 5.0) -> dict:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            payload = response.read()
    except (urllib.error.URLError, OSError) as exc:
        raise StatsError(f"cannot fetch allocator stats {url}: {exc}") from exc
    try:
        document = json.loads(payload)
    except json.JSONDecodeError as exc:
        raise StatsError(f"allocator stats {url} is not JSON: {exc}") from exc
    if not isinstance(document, dict):
        raise StatsError(f"allocator stats {url} root is not an object")
    return document


def read(source: str, required_keys=DEFAULT_REQUIRED_KEYS) -> dict:
    if source.startswith(("http://", "https://")):
        document = read_url(source)
    else:
        if not os.path.isfile(source):
            raise StatsError(f"allocator stats source does not exist: {source}")
        document = read_file(source)
    return normalize(document, required_keys)


def parse_required_keys(value: str) -> tuple[str, ...]:
    keys = tuple(part.strip() for part in value.split(",") if part.strip())
    if not keys:
        raise StatsError("--require-stats-keys must name at least one field")
    return keys
