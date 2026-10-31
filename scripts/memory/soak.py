#!/usr/bin/env python3
"""Issue 34 memory soak orchestrator.

Runs a target under continuous real TLS/HTTP traffic, samples Linux memory
statistics from ``/proc`` and cgroup v2, runs optional 2 GiB/no-swap pressure
cycles, and fails unless the post-warm-up RSS slope's upper 95% confidence
bound is below the configured budget.

Scope is explicit. ``bench-defense`` is the full Edge data plane
(``EdgeProxy`` + ``HttpProxyManager`` + real listeners) with local config
injection, but it is not the control-plane-managed production process. A
``bench-proxy`` target is rejected unless the caller explicitly opts into a
surrogate, and the report always records ``scope.full_edge`` and a note.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import threading
import time
import traceback

from . import loadgen, origin, pressure, procfs, slope, stats, status, verify_tls

MIB = 1024 * 1024
GIB = 1024 * MIB
DEFAULT_TLS_HOST = "localhost"
MAX_VERIFY_RESULTS = 64
MAX_VIOLATION_MESSAGES = 32
# The measurement window starts only after the load generator has produced a
# continuous clean 2xx interval. Errors under memory.high are expected pressure
# admission rejections and are accounted to the pressure window instead.
MIN_CLEAN_LOAD_SECONDS = 10.0
# Pressure without swap reclaims part of the target's working set. When the
# cgroup is released, the resident set refills to its steady state; that ramp
# is not the slow leak the MiB/h budget describes, so the measured window waits
# for the trailing RSS rate to flatten first. Bounded so a target that never
# plateaus is a reported violation rather than an indefinitely delayed run.
STABILIZATION_WINDOW_SECONDS = 30.0
STABILIZATION_MAX_RATE_MIB_PER_MIN = 8.0
STABILIZATION_REQUIRED_WINDOWS = 2
STABILIZATION_TIMEOUT_SECONDS = 180.0
# The controller proxies resourceGovernor from its first health snapshot, so
# the object can be briefly absent at startup even though the node is healthy.
STATUS_READY_TIMEOUT_SECONDS = 60.0


class SoakError(RuntimeError):
    """The soak could not be started or a strict contract was violated."""


def _resolve_status_fields(source: str | None, value: str | None) -> tuple[str, ...]:
    """Resolve the node-status field contract without ever silently emptying it.

    ``full_edge.py`` passes the defaults explicitly, but a direct
    ``run_soak.py --status-source URL`` must not crash (or skip validation)
    just because ``--require-status-fields`` was omitted.
    """
    if not source:
        return ()
    if value:
        return status.parse_required_fields(value)
    return status.DEFAULT_REQUIRED_FIELDS


def node_status_outcome(
    last: dict | None,
    samples: int,
    required_samples: int,
) -> tuple[list[str], dict | None]:
    """Validate the required node counters and report worker-coverage gaps.

    ``--require-node-status`` means the real counters must be present and
    live; it does **not** mean every Pingora worker is hooked. The real Edge
    honestly reports ``coversAllWorkers=false`` / ``unhookedPingoraWorkers>0``
    until the production hook lands, so that is reported as an explicit
    coverage gap instead of being silently skipped or treated as full coverage.
    """
    violations: list[str] = []
    gap: dict | None = None
    if last is None:
        violations.append("--require-node-status was set but no node status was read")
        return violations, gap
    if samples < required_samples:
        violations.append(
            f"only {samples} node status samples, need {required_samples}"
        )
    # Field presence is enforced for every polled sample by status.load();
    # monotonic generation/cycles are enforced in poll_status(). Do not invent
    # a nonzero threshold here: a healthy Edge may legitimately report
    # generation=0 when no worker reclaim has occurred yet.
    if not last["workerReclaim.coversAllWorkers"]:
        gap = {
            "field": "workerReclaim.coversAllWorkers",
            "value": False,
            "unhookedPingoraWorkers": last.get(
                "workerReclaim.unhookedPingoraWorkers"
            ),
            "note": (
                "required node counters were present and live, but not every "
                "Pingora worker is hooked; this is a reported coverage gap, "
                "not full-worker coverage"
            ),
        }
    return violations, gap


def _target_scope(kind: str, command: str, allow_bench_proxy: bool) -> dict:
    base = os.path.basename(command)
    if "bench-proxy" in base or kind == "bench-proxy":
        if not allow_bench_proxy:
            raise SoakError(
                "refusing to soak bench-proxy as if it were Edge; pass "
                "--allow-bench-proxy-surrogate to record an explicit surrogate run"
            )
        return {
            "kind": "bench-proxy-surrogate",
            "component_lane": False,
            "production_startup": False,
            "full_edge": False,
            "note": (
                "bench-proxy is a minimal HTTP proxy, not the full Edge request "
                "pipeline; this run is an explicit allocator surrogate only."
            ),
        }
    if kind == "bench-defense":
        return {
            "kind": "bench-defense-component",
            "component_lane": True,
            "production_startup": False,
            "full_edge": False,
            "note": (
                "COMPONENT LANE ONLY: bench-defense constructs EdgeProxy + "
                "HttpProxyManager with real HTTP/TLS listeners and local config "
                "injection. It exercises the same allocation and handoff paths, but "
                "it is not the production startup path (no control-plane config, "
                "RPC node client, or production listener wiring) and must never be "
                "reported as full-Edge coverage."
            ),
        }
    if kind == "full-edge":
        if base not in ("Zelvik-Edge", "zelvik-edge"):
            raise SoakError(
                "target-kind full-edge requires the production Zelvik-Edge binary, "
                f"got {base!r}; bench-defense is a component lane"
            )
        return {
            "kind": "production-edge",
            "component_lane": False,
            "production_startup": True,
            "full_edge": True,
            "note": (
                "production Zelvik-Edge binary started from a local ZELVIK_EDGE_HOME "
                "by a caller-supplied production-startup fixture. That fixture is "
                "owned separately; this harness only labels the scope."
            ),
        }
    return {
        "kind": kind,
        "component_lane": False,
        "production_startup": False,
        "full_edge": False,
        "note": "caller-declared target; full-Edge scope was not asserted",
    }


def _wait_tcp(host: str, port: int, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            with socket.create_connection((host, port), timeout=2.0):
                return
        except OSError as exc:
            last_error = exc
            time.sleep(0.2)
    raise SoakError(f"target did not accept connections on {host}:{port}: {last_error}")


def _wait_pid_file(path: str, timeout: float) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.isfile(path):
            return procfs.read_pid_file(path)
        time.sleep(0.1)
    raise SoakError(f"target pid file {path} did not appear within {timeout}s")


def _env_from_args(values) -> dict:
    env = dict(os.environ)
    for item in values or []:
        key, sep, value = item.partition("=")
        if not sep or not key:
            raise SoakError(f"--target-env must be KEY=VALUE, got {item!r}")
        env[key] = value
    return env


def _write_jsonl(handle, record: dict) -> None:
    handle.write(json.dumps(record, sort_keys=True) + "\n")
    handle.flush()


def _stats_sample(source: str | None, required_keys) -> dict | None:
    if not source:
        return None
    return stats.read(source, required_keys)


def _wait_stats_ready(
    source: str,
    required_keys,
    timeout_seconds: float,
) -> dict:
    """Wait for the target's allocator stats surface before measuring.

    The target writes ``alloc-stats.json`` from its own periodic loop, so the
    file is not present at the instant the soak starts. A bounded readiness
    handshake replaces a synchronous first read: missing/partial/incomplete
    stats are retried, but a target that never publishes them fails within a
    fixed window instead of after the whole soak.
    """
    deadline = time.monotonic() + timeout_seconds
    last_error = None
    while True:
        try:
            return stats.read(source, required_keys)
        except stats.StatsError as exc:
            last_error = exc
        if time.monotonic() >= deadline:
            raise SoakError(
                "allocator stats did not become ready within "
                f"{timeout_seconds:g}s: {last_error}"
            )
        time.sleep(min(0.2, max(0.0, deadline - time.monotonic())))


def _wait_status_ready(
    source: str,
    required_fields,
    timeout_seconds: float,
    poll_interval: float = 0.5,
    clock=None,
    sleep=None,
) -> dict:
    """Wait for the first complete ``resourceGovernor`` snapshot at startup.

    The controller populates the governor object from its first health
    snapshot, so a single missing object right after the target starts is a
    startup race, not a contract failure. Retry within a fixed bound; after
    the bound the absence is surfaced as a violation by the caller. Later
    polls are strict again.
    """
    clock = clock or time.monotonic
    sleep = sleep or time.sleep
    if timeout_seconds < 0:
        raise SoakError("status readiness timeout must be >= 0")
    deadline = clock() + timeout_seconds
    last_error = None
    while True:
        try:
            return status.load(source, required_fields)
        except status.StatusError as exc:
            last_error = exc
        if clock() >= deadline:
            raise status.StatusError(
                "resourceGovernor did not become available within "
                f"{timeout_seconds:g}s: {last_error}"
            )
        sleep(poll_interval)


def _wait_clean_load(
    runner,
    *,
    min_clean_seconds: float,
    timeout_seconds: float,
    poll_interval: float = 0.5,
    clock=None,
    sleep=None,
) -> dict:
    """Wait for a continuous clean-2xx interval after the pressure block.

    Errors are expected while the cgroup is above ``memory.high`` (503
    admission rejections). The measurement window must not begin until the
    generator has recovered, otherwise the post-pressure tail is misreported as
    a measurement-window failure. Any error resets the clean window. The total
    wait is bounded; a target that never recovers is a hard failure.
    """
    clock = clock or time.monotonic
    sleep = sleep or time.sleep
    if min_clean_seconds < MIN_CLEAN_LOAD_SECONDS:
        raise SoakError(
            f"clean-load window must be at least {MIN_CLEAN_LOAD_SECONDS:g}s, "
            f"got {min_clean_seconds:g}s"
        )
    if timeout_seconds <= 0:
        raise SoakError("clean-load handshake timeout must be positive")

    deadline = clock() + timeout_seconds
    initial = runner.summary()
    last_errors = initial["errors"]
    window_start = clock()
    window_ok_start = initial["requests_ok"]
    while True:
        sleep(poll_interval)
        now = clock()
        current = runner.summary()
        if current["errors"] > last_errors:
            last_errors = current["errors"]
            window_start = now
            window_ok_start = current["requests_ok"]
        if (
            now - window_start >= min_clean_seconds
            and current["requests_ok"] > window_ok_start
        ):
            return {
                "seconds": now - window_start,
                "end": now,
                "errors": current["errors"] - initial["errors"],
                "requests_ok": current["requests_ok"] - initial["requests_ok"],
            }
        if now >= deadline:
            raise SoakError(
                "load generator did not produce clean 2xx for "
                f"{min_clean_seconds:g}s within {timeout_seconds:g}s "
                f"(errors={current['errors'] - initial['errors']}, "
                f"requests_ok={current['requests_ok'] - initial['requests_ok']})"
            )


def _wait_stabilization(
    capture,
    *,
    window_seconds: float = STABILIZATION_WINDOW_SECONDS,
    max_rate_mib_per_min: float = STABILIZATION_MAX_RATE_MIB_PER_MIN,
    required_windows: int = STABILIZATION_REQUIRED_WINDOWS,
    timeout_seconds: float = STABILIZATION_TIMEOUT_SECONDS,
    poll_interval: float = 1.0,
    clock=None,
    sleep=None,
    on_sample=None,
) -> dict:
    """Wait for RSS to plateau after pressure before starting the measurement.

    Pressure under ``memory.high`` without swap evicts/reclaims part of the
    target's working set. After release the resident set ramps back to steady
    state, and a window that starts immediately would measure that refill as a
    fake leak. Stabilization is reached when the trailing ``window_seconds``
    absolute RSS delta rate is at or below ``max_rate_mib_per_min`` for
    ``required_windows`` complete window spans. Overlapping polls do not count
    as additional windows. The wait is bounded; the
    caller turns a non-plateau into an explicit violation, never a skip.

    ``on_sample`` receives every captured sample so the caller can keep one
    continuous sample series (these points are still before
    ``measurement_start`` and therefore excluded from the slope).
    """
    clock = clock or time.monotonic
    sleep = sleep or time.sleep
    if window_seconds <= 0:
        raise SoakError("stabilization window must be positive")
    if required_windows < 1:
        raise SoakError("stabilization requires at least one window")
    if max_rate_mib_per_min < 0:
        raise SoakError("stabilization rate limit must be >= 0")
    if timeout_seconds < 0:
        raise SoakError("stabilization timeout must be >= 0")

    points: list[tuple[float, int]] = []

    def take() -> None:
        sample = capture()
        points.append((clock(), sample.rss_bytes))
        if on_sample is not None:
            on_sample(sample)

    started_at = clock()
    deadline = started_at + timeout_seconds
    take()
    consecutive = 0
    stable_since = None
    last_rate = None
    while True:
        sleep(poll_interval)
        now = clock()
        take()
        newest_t, newest = points[-1]
        window_start = None
        for t, value in points:
            if newest_t - t >= window_seconds:
                window_start = (t, value)
            else:
                break
        if window_start is not None:
            elapsed = newest_t - window_start[0]
            last_rate = (newest - window_start[1]) / elapsed * 60.0 / MIB
            if abs(last_rate) <= max_rate_mib_per_min:
                if stable_since is None:
                    stable_since = window_start[0]
                consecutive = int((newest_t - stable_since) / window_seconds)
            else:
                stable_since = None
                consecutive = 0
            if consecutive >= required_windows:
                return {
                    "seconds": now - started_at,
                    "end": now,
                    "plateaued": True,
                    "plateau_rate_mib_per_min": last_rate,
                    "windows": consecutive,
                    "samples": len(points),
                    "window_seconds": window_seconds,
                    "max_rate_mib_per_min": max_rate_mib_per_min,
                    "timeout_seconds": timeout_seconds,
                }
        if now >= deadline:
            return {
                "seconds": now - started_at,
                "end": now,
                "plateaued": False,
                "plateau_rate_mib_per_min": last_rate,
                "windows": consecutive,
                "samples": len(points),
                "window_seconds": window_seconds,
                "max_rate_mib_per_min": max_rate_mib_per_min,
                "timeout_seconds": timeout_seconds,
            }


def _measurement_start(
    started: float,
    warmup_seconds: float,
    pressure_end: float | None,
    handshake_end: float | None,
    stabilization_end: float | None = None,
) -> float:
    """Resolve the measurement start and prove pressure finished before it.

    The warm-up floor still applies to long lanes (their pressure block starts
    early inside the warm-up), while the fast lanes start measuring only after
    the post-pressure clean-load handshake and the RSS stabilization plateau.
    There is no fallback that silently includes pressure/recovery samples in
    the measured window.
    """
    candidates = [started + warmup_seconds]
    if handshake_end is not None:
        candidates.append(handshake_end)
    if stabilization_end is not None:
        candidates.append(stabilization_end)
    measurement_start = max(candidates)
    if pressure_end is not None and pressure_end >= measurement_start:
        raise SoakError(
            "pressure block ended after the measurement window started: "
            f"pressure_end={pressure_end - started:.3f}s, "
            f"measurement_start={measurement_start - started:.3f}s"
        )
    return measurement_start


def _effective_warmup(
    samples: list[tuple[float, int]],
    warmup_seconds: float,
    last_pressure_end: float | None,
    pressure_settle_seconds: float,
) -> float:
    effective = warmup_seconds
    if last_pressure_end is not None:
        effective = max(
            effective, last_pressure_end - samples[0][0] + pressure_settle_seconds
        )
    return effective


def _measurement_points(
    samples: list[tuple[float, int]], measurement_start: float | None
) -> tuple[list[tuple[float, int]], float | None]:
    """Select the measured samples and report the start offset.

    When ``measurement_start`` is given, the slope is computed on exactly that
    window with no second warm-up. This is what removes the old correctness
    dependency on ``last_pressure_end``: pressure and the recovery handshake
    are simply outside the selected points.
    """
    if measurement_start is None:
        return samples, None
    selected = [(t, value) for t, value in samples if t >= measurement_start]
    effective = max(0.0, measurement_start - samples[0][0]) if samples else 0.0
    return selected, effective


def _relabel_error(message: str, label: str) -> str:
    """Keep RSS/Anon gate diagnostics independent without duplicating slope.py."""
    if label == "RSS":
        return message
    return message.replace("RSS", label)


_SLOPE_METRIC_KEYS = (
    "samples",
    "warmup_seconds",
    "span_seconds",
    "slope_bytes_per_second",
    "upper95_bytes_per_second",
    "slope_mib_per_hour",
    "upper95_mib_per_hour",
    "theil_sen_mib_per_hour",
    "r_squared",
    "t_critical",
)


def _diagnostic_slope(
    points,
    *,
    warmup_seconds: float,
    min_span_seconds: float,
) -> dict | None:
    """Recompute point/robust/upper values after a budget failure.

    ``slope.estimate_slope`` raises on a budget violation without returning
    the metrics, which previously left the failing gate with only the message.
    Re-running with an infinite budget recovers the independent RSS/Anon
    point, robust and upper values for the report; the gate itself still fails.
    """
    try:
        return slope.estimate_slope(
            points,
            warmup_seconds=warmup_seconds,
            budget_mib_per_hour=float("inf"),
            min_span_seconds=min_span_seconds,
        ).to_dict()
    except slope.ThresholdError:
        return None


def analyze(
    samples: list[tuple[float, int]],
    *,
    warmup_seconds: float,
    budget_mib_per_hour: float,
    min_span_seconds: float,
    last_pressure_end: float | None,
    pressure_settle_seconds: float,
    measurement_start: float | None = None,
    label: str = "RSS",
) -> dict:
    """Compute the gate slope. Returns ``{"ok": bool, ...}`` and never raises."""
    if not samples:
        return {"ok": False, "error": "no samples", "label": label}
    if measurement_start is not None:
        points, effective = _measurement_points(samples, measurement_start)
        slope_warmup = 0.0
    else:
        points = samples
        effective = _effective_warmup(
            samples, warmup_seconds, last_pressure_end, pressure_settle_seconds
        )
        slope_warmup = effective
    try:
        result = slope.estimate_slope(
            points,
            warmup_seconds=slope_warmup,
            budget_mib_per_hour=budget_mib_per_hour,
            min_span_seconds=min_span_seconds,
        )
    except slope.ThresholdError as exc:
        payload = {
            "ok": False,
            "error": _relabel_error(str(exc), label),
            "effective_warmup_seconds": effective,
            "measurement_start": measurement_start,
            "label": label,
        }
        diagnostics = _diagnostic_slope(
            points,
            warmup_seconds=slope_warmup,
            min_span_seconds=min_span_seconds,
        )
        if diagnostics is not None:
            for key in _SLOPE_METRIC_KEYS:
                payload[key] = diagnostics[key]
            payload["budget_mib_per_hour"] = budget_mib_per_hour
        return payload
    return {
        "ok": True,
        **result.to_dict(),
        "effective_warmup_seconds": effective,
        "measurement_start": measurement_start,
        "label": label,
    }


def analyze_split(
    samples: list[tuple[float, int]],
    *,
    warmup_seconds: float,
    budget_mib_per_hour: float,
    min_span_seconds: float,
    last_pressure_end: float | None,
    pressure_settle_seconds: float,
    split_fraction: float = 0.5,
    measurement_start: float | None = None,
    label: str = "RSS",
) -> dict:
    """Second-half re-gate plus step baseline for long dedicated runs."""
    if not samples:
        return {"ok": False, "error": "no samples", "label": label}
    if measurement_start is not None:
        points, effective = _measurement_points(samples, measurement_start)
        slope_warmup = 0.0
    else:
        points = samples
        effective = _effective_warmup(
            samples, warmup_seconds, last_pressure_end, pressure_settle_seconds
        )
        slope_warmup = effective
    try:
        result = slope.estimate_split_slope(
            points,
            warmup_seconds=slope_warmup,
            budget_mib_per_hour=budget_mib_per_hour,
            min_span_seconds=min_span_seconds,
            split_fraction=split_fraction,
        )
    except slope.ThresholdError as exc:
        return {
            "ok": False,
            "error": _relabel_error(str(exc), label),
            "effective_warmup_seconds": effective,
            "measurement_start": measurement_start,
            "label": label,
        }
    return {
        "ok": True,
        **result.to_dict(),
        "effective_warmup_seconds": effective,
        "measurement_start": measurement_start,
        "label": label,
    }


def analyze_pair(
    rss_samples: list[tuple[float, int]],
    anon_samples: list[tuple[float, int]],
    *,
    warmup_seconds: float,
    budget_mib_per_hour: float,
    anon_budget_mib_per_hour: float,
    min_span_seconds: float,
    last_pressure_end: float | None,
    pressure_settle_seconds: float,
    measurement_start: float | None = None,
) -> dict:
    """Gate RSS and anonymous RSS independently.

    A flat total RSS can hide anonymous-memory growth offset by file-page
    reclaim; the Issue 34 gate therefore requires *both* series to stay under
    the budget. There is no combined/lenient fallback. The two label/values are
    kept independent so an Anon failure never prints the RSS series.
    """
    rss = analyze(
        rss_samples,
        warmup_seconds=warmup_seconds,
        budget_mib_per_hour=budget_mib_per_hour,
        min_span_seconds=min_span_seconds,
        last_pressure_end=last_pressure_end,
        pressure_settle_seconds=pressure_settle_seconds,
        measurement_start=measurement_start,
        label="RSS",
    )
    anon = analyze(
        anon_samples,
        warmup_seconds=warmup_seconds,
        budget_mib_per_hour=anon_budget_mib_per_hour,
        min_span_seconds=min_span_seconds,
        last_pressure_end=last_pressure_end,
        pressure_settle_seconds=pressure_settle_seconds,
        measurement_start=measurement_start,
        label="Anon",
    )
    return {
        "ok": bool(rss.get("ok")) and bool(anon.get("ok")),
        "rss": rss,
        "anon": anon,
        "budgets": {
            "rss_mib_per_hour": budget_mib_per_hour,
            "anon_mib_per_hour": anon_budget_mib_per_hour,
        },
    }


def run(args) -> int:
    if args.duration_seconds <= 0:
        raise SoakError("--duration must be positive")
    if args.sample_interval <= 0:
        raise SoakError("--sample-interval must be positive")
    if args.max_samples < 100:
        raise SoakError("--max-samples must be at least 100")
    if args.require_stats_samples < 0:
        raise SoakError("--require-stats-samples must be >= 0")
    if args.stats_ready_timeout_seconds < 0:
        raise SoakError("--stats-ready-timeout-seconds must be >= 0")
    if args.warmup_seconds < 0:
        raise SoakError("--warmup-seconds must be >= 0")
    if args.measurement_clean_seconds < MIN_CLEAN_LOAD_SECONDS:
        raise SoakError(
            "--measurement-clean-seconds must be at least "
            f"{MIN_CLEAN_LOAD_SECONDS:g}"
        )
    if (
        args.measurement_handshake_timeout_seconds
        < args.measurement_clean_seconds
    ):
        raise SoakError(
            "--measurement-handshake-timeout-seconds must be >= "
            "--measurement-clean-seconds"
        )
    if args.require_status_samples < 0:
        raise SoakError("--require-status-samples must be >= 0")
    if args.status_ready_timeout_seconds < 0:
        raise SoakError("--status-ready-timeout-seconds must be >= 0")
    if args.measurement_stabilize_timeout_seconds < 0:
        raise SoakError("--measurement-stabilize-timeout-seconds must be >= 0")
    if args.stats_source and args.require_allocator_stats:
        required_keys = (
            stats.parse_required_keys(args.require_stats_keys)
            if args.require_stats_keys
            else stats.DEFAULT_REQUIRED_KEYS
        )
    elif args.require_allocator_stats:
        raise SoakError("--require-allocator-stats requires --stats-source")
    else:
        required_keys = ()

    if args.require_node_status and not args.status_source:
        raise SoakError("--require-node-status requires --status-source")
    status_fields = _resolve_status_fields(
        args.status_source, args.require_status_fields
    )
    if args.status_interval < 0:
        raise SoakError("--status-interval must be >= 0")
    if args.budget_mib_per_hour <= 0:
        raise SoakError("--budget-mib-per-hour must be positive")
    anon_budget = (
        args.anon_budget_mib_per_hour
        if args.anon_budget_mib_per_hour is not None
        else args.budget_mib_per_hour
    )
    if anon_budget <= 0:
        raise SoakError("--anon-budget-mib-per-hour must be positive")
    if args.split_baseline and args.duration_seconds - args.warmup_seconds <= 0:
        raise SoakError("--split-baseline requires a post-warm-up window")
    if args.pressure_recovery_probe_seconds < 0:
        raise SoakError("--pressure-recovery-probe-seconds must be >= 0")

    scope = _target_scope(args.target_kind, args.target_cmd, args.allow_bench_proxy_surrogate)

    violations: list[str] = []
    def violate(message: str) -> None:
        if len(violations) < MAX_VIOLATION_MESSAGES:
            violations.append(message)
        else:
            violations.append("... additional violations truncated")

    cgroup_ctx = None
    if args.pressure_cycles > 0:
        cgroup_ctx = pressure.ensure_context(
            require_memory_max=args.pressure_max_bytes,
            require_swap_max=args.pressure_swap_max,
            require_high=True,
            cgroup_root=args.cgroup_root,
        )

    progress = pressure.Progress(args.progress_file)
    origin_server = None
    target = None
    target_log = None
    runner = None
    verifier_stop = threading.Event()
    verifier_thread = None
    verify_results: list[dict] = []
    verify_errors: list[str] = []
    # Periodic TLS verification must not overlap the pressure block: 503
    # admission rejections are expected there, while the TLS contract itself
    # (CA, SNI, ALPN h2, payload echo) is still checked strictly outside it.
    # The lock serializes a check that is already running with the pause, so a
    # check cannot slip into the pressure window at the boundary.
    verifier_pause = threading.Event()
    verify_lock = threading.Lock()
    paused_verifications = 0

    def verify_once(phase: str) -> None:
        try:
            result = verify_tls.verify(
                host=args.tls_host,
                port=args.tls_port,
                ca_file=args.tls_ca,
                path=args.tls_path,
                payload=loadgen_payload,
            )
        except (verify_tls.TlsVerifyError, OSError) as exc:
            message = f"{phase}: {exc}"
            if len(verify_errors) < MAX_VIOLATION_MESSAGES:
                verify_errors.append(message)
            violate(message)
            return
        if len(verify_results) < MAX_VERIFY_RESULTS:
            verify_results.append({"phase": phase, "monotonic_s": time.monotonic(), **result.to_dict()})

    def verifier_loop() -> None:
        nonlocal paused_verifications
        while not verifier_stop.wait(args.tls_verify_interval):
            with verify_lock:
                if verifier_pause.is_set():
                    paused_verifications += 1
                    continue
                verify_once("periodic")

    loadgen_payload = (args.load_payload or "issue34-load-payload\n").encode() * 256
    if not loadgen_payload or len(loadgen_payload) > MIB:
        raise SoakError("load payload must be between 1 byte and 1 MiB")

    try:
        if args.origin == "auto":
            origin_server = origin.OriginServer(
                host="127.0.0.1",
                port=args.origin_port,
                payload_bytes=args.origin_payload_bytes,
            ).start()
            args.target_env = list(args.target_env or [])
            args.target_env.append(f"BENCH_ORIGIN_HTTP=127.0.0.1:{origin_server.port}")
            args.target_env.append(f"BENCH_ORIGIN_UDP=127.0.0.1:{args.origin_udp_port}")
        env = _env_from_args(args.target_env)
        command = [args.target_cmd, *args.target_args]
        if args.target_log:
            target_log = open(args.target_log, "w", encoding="utf-8")
        target = subprocess.Popen(
            command,
            env=env,
            stdout=target_log if target_log is not None else None,
            stderr=subprocess.STDOUT if target_log is not None else None,
        )
        if args.target_pid_file:
            pid = _wait_pid_file(args.target_pid_file, args.ready_timeout_seconds)
        else:
            pid = target.pid
        sampler = procfs.Sampler(pid)
        _wait_tcp("127.0.0.1", args.tls_port, args.ready_timeout_seconds)
        verify_once("initial")
        if verify_errors:
            raise SoakError(f"initial TLS/HTTP2/SNI verification failed: {verify_errors[0]}")

        initial = sampler.capture()
        if initial.cgroup_swap_current_bytes != 0:
            violate(
                f"swap in use at start: {initial.cgroup_swap_current_bytes} bytes"
            )
        if initial.rss_bytes > args.max_rss_mib * MIB:
            violate(
                f"initial RSS {initial.rss_bytes / MIB:.1f} MiB exceeds "
                f"{args.max_rss_mib:.0f} MiB"
            )
        stats_samples = 0
        stats_last = None
        if required_keys:
            # The target publishes stats from its own 1 s loop; wait for the
            # first complete document before starting the measurement clock.
            stats_last = _wait_stats_ready(
                args.stats_source,
                required_keys,
                args.stats_ready_timeout_seconds,
            )
            stats_samples = 1
        started = time.monotonic()

        if args.tls_verify_interval > 0:
            verifier_thread = threading.Thread(
                target=verifier_loop, name="issue34-tls-verifier", daemon=True
            )
            verifier_thread.start()

        runner = loadgen.LoadRunner(
            loadgen.LoadConfig(
                sni=args.tls_host,
                port=args.tls_port,
                ca_file=args.tls_ca,
                path=args.load_path,
                concurrency=args.load_concurrency,
                duration_seconds=args.duration_seconds,
                max_requests=args.max_requests,
                method=args.load_method,
                payload=loadgen_payload if args.load_method == "POST" else b"",
                timeout_seconds=args.request_timeout_seconds,
            )
        ).start()

        samples: list[tuple[float, int]] = []
        anon_samples: list[tuple[float, int]] = []
        peak_rss = initial.rss_bytes
        last_pressure_end = None
        pressure_summary = None
        pressure_blocked_seconds = 0.0
        pressure_load_baseline = 0
        pressure_load_errors = 0
        measurement_handshake = None
        measurement_stabilization = None
        measurement_start = None
        measurement_target = (
            None
            if args.pressure_cycles > 0
            else started + args.warmup_seconds
        )
        measurement_load_baseline = None
        cycle_done = False
        cycle_at = args.duration_seconds * args.pressure_start_fraction
        jsonl = open(args.samples_jsonl, "w", encoding="utf-8") if args.samples_jsonl else None

        def record_sample(sample) -> None:
            """Append one RSS/Anon point and enforce the per-sample bounds.

            Used by the main loop and by the post-pressure stabilization wait,
            so the sample series stays continuous across both.
            """
            nonlocal peak_rss, stats_last, stats_samples
            samples.append((sample.monotonic_s, sample.rss_bytes))
            anon_samples.append((sample.monotonic_s, sample.anon_bytes))
            if len(samples) > args.max_samples:
                raise SoakError(
                    f"sample array reached the {args.max_samples} bound; "
                    "increase --sample-interval or --max-samples"
                )
            peak_rss = max(peak_rss, sample.rss_bytes)
            if sample.cgroup_swap_current_bytes != 0:
                violate(
                    f"swap in use during run: {sample.cgroup_swap_current_bytes} bytes"
                )
            if sample.rss_bytes > args.max_rss_mib * MIB:
                violate(
                    f"RSS {sample.rss_bytes / MIB:.1f} MiB exceeds "
                    f"{args.max_rss_mib:.0f} MiB"
                )
            if jsonl is not None:
                _write_jsonl(jsonl, {"kind": "sample", **sample.to_dict()})
            if required_keys:
                stats_last = _stats_sample(args.stats_source, required_keys)
                stats_samples += 1
                if jsonl is not None:
                    _write_jsonl(
                        jsonl,
                        {
                            "kind": "allocator",
                            "monotonic_s": sample.monotonic_s,
                            "stats": stats_last,
                        },
                    )

        status_first = None
        status_last = None
        status_samples = 0
        status_errors: list[str] = []
        previous_status = None
        next_status_at = 0.0

        def poll_status(ready_wait: bool = False) -> None:
            nonlocal status_first, status_last, status_samples, previous_status
            try:
                if ready_wait and args.status_ready_timeout_seconds > 0:
                    values = _wait_status_ready(
                        args.status_source,
                        status_fields,
                        args.status_ready_timeout_seconds,
                    )
                else:
                    values = status.load(args.status_source, status_fields)
            except status.StatusError as exc:
                message = f"node status: {exc}"
                if len(status_errors) < MAX_VIOLATION_MESSAGES:
                    status_errors.append(message)
                violate(message)
                return
            status_samples += 1
            if status_first is None:
                status_first = values
            if previous_status is not None:
                for counter in ("workerReclaim.generation", "cacheJanitor.cycles"):
                    if values[counter] < previous_status[counter]:
                        violate(f"node status {counter} decreased during the run")
            previous_status = values
            status_last = values
            if jsonl is not None:
                _write_jsonl(
                    jsonl,
                    {
                        "kind": "node_status",
                        "monotonic_s": time.monotonic(),
                        "resourceGovernor": values,
                    },
                )

        if args.status_source:
            # Bounded startup readiness: the controller proxies
            # resourceGovernor from its first health snapshot.
            poll_status(ready_wait=True)
            next_status_at = args.status_interval

        try:
            while True:
                elapsed = time.monotonic() - started
                if elapsed >= args.duration_seconds:
                    break
                if target.poll() is not None:
                    raise SoakError(
                        f"target exited early with status {target.returncode}"
                    )
                sample = sampler.capture()
                record_sample(sample)
                if args.status_source and elapsed >= next_status_at:
                    poll_status()
                    next_status_at = elapsed + args.status_interval
                if (
                    measurement_target is not None
                    and measurement_start is None
                    and time.monotonic() >= measurement_target
                ):
                    measurement_start = measurement_target
                    measurement_load_baseline = runner.summary()
                if (
                    args.pressure_cycles > 0
                    and not cycle_done
                    and elapsed >= cycle_at
                ):
                    # Pressure cycles are synchronous: the fork-based
                    # allocator must not fork a multi-threaded process, so the
                    # sampling loop is blocked while they run. They are
                    # scheduled at the warm-up boundary and finish before the
                    # measured window; the report proves the ordering instead
                    # of relying on a post-hoc sample exclusion.
                    with verify_lock:
                        verifier_pause.set()
                    pressure_started = time.monotonic()
                    pressure_load_baseline = runner.summary()["errors"]
                    cycles = pressure.run_cycles(
                        sampler,
                        cycles=args.pressure_cycles,
                        pressure_bytes=args.pressure_bytes,
                        hold_seconds=args.pressure_hold_seconds,
                        settle_timeout_seconds=args.pressure_settle_seconds,
                        progress=progress,
                        require_events=True,
                        recovery_probe_seconds=args.pressure_recovery_probe_seconds,
                        long_recovery=args.pressure_long_recovery,
                    )
                    pressure_blocked_seconds += time.monotonic() - pressure_started
                    last_pressure_end = time.monotonic()
                    pressure_load_errors = (
                        runner.summary()["errors"] - pressure_load_baseline
                    )
                    cycle_done = True
                    pressure_summary = {
                        key: cycles[key]
                        for key in (
                            "cycles",
                            "pressure_bytes",
                            "hold_seconds",
                            "settle_timeout_seconds",
                            "recovery_probe_seconds",
                            "recovery_probe_samples",
                            "long_recovery",
                            "event_deltas",
                            "same_pid",
                            "initial_baseline_current_bytes",
                            "recovery_baseline_current_bytes",
                            "recovery_slack_bytes",
                        )
                    }
                    # Do not start measuring on the 503 tail. The load
                    # generator must show a continuous clean-2xx window after
                    # pressure; any error restarts that window.
                    measurement_handshake = _wait_clean_load(
                        runner,
                        min_clean_seconds=args.measurement_clean_seconds,
                        timeout_seconds=args.measurement_handshake_timeout_seconds,
                    )
                    # Releasing the cgroup lets the target refill the working
                    # set that pressure reclaimed. That ramp is not leak
                    # growth, so the measured window starts only once the
                    # trailing RSS rate has flattened (bounded).
                    measurement_stabilization = _wait_stabilization(
                        sampler.capture,
                        timeout_seconds=args.measurement_stabilize_timeout_seconds,
                        on_sample=record_sample,
                    )
                    if not measurement_stabilization["plateaued"]:
                        rate = measurement_stabilization["plateau_rate_mib_per_min"]
                        rate_text = (
                            "no complete "
                            f"{measurement_stabilization['window_seconds']:g}s "
                            "window"
                            if rate is None
                            else f"{rate:.2f} MiB/min"
                        )
                        violate(
                            "rss did not stabilize after pressure within "
                            f"{measurement_stabilization['timeout_seconds']:g}s "
                            f"(trailing "
                            f"{measurement_stabilization['window_seconds']:g}s "
                            f"rate {rate_text}, limit "
                            f"{measurement_stabilization['max_rate_mib_per_min']:g} "
                            "MiB/min)"
                        )
                    measurement_target = _measurement_start(
                        started,
                        args.warmup_seconds,
                        last_pressure_end,
                        measurement_handshake["end"],
                        measurement_stabilization["end"],
                    )
                    if measurement_target <= time.monotonic():
                        measurement_start = measurement_target
                        measurement_load_baseline = runner.summary()
                    if jsonl is not None:
                        _write_jsonl(jsonl, {"kind": "pressure", **cycles})
                        _write_jsonl(
                            jsonl,
                            {
                                "kind": "measurement_handshake",
                                **measurement_handshake,
                            },
                        )
                        _write_jsonl(
                            jsonl,
                            {
                                "kind": "measurement_stabilization",
                                **measurement_stabilization,
                            },
                        )
                    with verify_lock:
                        verifier_pause.clear()
                time.sleep(args.sample_interval)
        finally:
            if jsonl is not None:
                jsonl.close()

        if runner is not None:
            runner.stop()
            runner.join(timeout=30)
        verifier_stop.set()
        if verifier_thread is not None:
            verifier_thread.join(timeout=10)

        if args.pressure_cycles > 0 and not cycle_done:
            violate("pressure cycles were requested but never ran")

        # If the loop ended before the scheduled measurement boundary (for
        # example a run shorter than its warm-up), keep the report explicit.
        # The slope gate will still fail for the missing/empty window.
        if measurement_start is None:
            measurement_start = (
                measurement_target
                if measurement_target is not None
                else started + args.warmup_seconds
            )
            measurement_load_baseline = runner.summary() if runner else {}

        load_summary = runner.summary() if runner else {}
        if measurement_load_baseline is None:
            measurement_load_baseline = load_summary
        pre_measurement_load_errors = measurement_load_baseline.get("errors", 0)
        measurement_load_errors = max(
            0, load_summary.get("errors", 0) - pre_measurement_load_errors
        )
        measurement_requests_ok = max(
            0,
            load_summary.get("requests_ok", 0)
            - measurement_load_baseline.get("requests_ok", 0),
        )
        pressure_end_offset = (
            None if last_pressure_end is None else last_pressure_end - started
        )
        measurement_start_offset = measurement_start - started
        measurement_handshake_end_offset = (
            None
            if measurement_handshake is None
            else measurement_handshake["end"] - started
        )
        measurement_stabilization_end_offset = (
            None
            if measurement_stabilization is None
            else measurement_stabilization["end"] - started
        )
        measurement_stabilization_seconds = (
            measurement_stabilization["seconds"]
            if measurement_stabilization is not None
            else 0.0
        )
        measurement_plateau_rate = (
            measurement_stabilization["plateau_rate_mib_per_min"]
            if measurement_stabilization is not None
            else None
        )
        measurement_plateaued = (
            measurement_stabilization["plateaued"]
            if measurement_stabilization is not None
            else None
        )
        if (
            measurement_handshake_end_offset is not None
            and measurement_handshake_end_offset > measurement_start_offset
        ):
            violate(
                "clean-load handshake ended after the measurement window "
                f"started: handshake_end={measurement_handshake_end_offset:.3f}s, "
                f"measurement_start={measurement_start_offset:.3f}s"
            )
        if (
            measurement_stabilization_end_offset is not None
            and measurement_stabilization_end_offset > measurement_start_offset
        ):
            violate(
                "rss stabilization ended after the measurement window started: "
                f"stabilization_end={measurement_stabilization_end_offset:.3f}s, "
                f"measurement_start={measurement_start_offset:.3f}s"
            )
        pressure_before_measurement = (
            last_pressure_end is None
            or (
                measurement_start is not None
                and last_pressure_end < measurement_start
            )
        )
        if last_pressure_end is not None and not pressure_before_measurement:
            violate(
                "pressure block ended after the measurement window started: "
                f"pressure_end={pressure_end_offset:.3f}s, "
                f"measurement_start={measurement_start_offset:.3f}s"
            )
        if load_summary.get("requests_ok", 0) < args.min_requests:
            violate(
                f"only {load_summary.get('requests_ok', 0)} successful requests, "
                f"need {args.min_requests}"
            )
        # Only measurement-window errors are budgeted. 503 admission
        # rejections while the cgroup is above memory.high are expected and are
        # reported as pressure_load_errors with no maximum.
        if measurement_load_errors and args.max_load_errors is not None:
            if measurement_load_errors > args.max_load_errors:
                violate(
                    f"{measurement_load_errors} measurement-window load errors "
                    f"exceed {args.max_load_errors} "
                    f"(pressure-window errors are separate: "
                    f"{pressure_load_errors})"
                )
        if required_keys and stats_samples < args.require_stats_samples:
            violate(
                f"only {stats_samples} allocator stats samples, need "
                f"{args.require_stats_samples}"
            )
        node_status_gap = None
        if args.require_node_status:
            status_violations, node_status_gap = node_status_outcome(
                status_last, status_samples, args.require_status_samples
            )
            for message in status_violations:
                violate(message)
            if node_status_gap is not None and args.require_complete_worker_coverage:
                violate(
                    "worker coverage gap: "
                    f"{node_status_gap['unhookedPingoraWorkers']} Pingora "
                    "workers are not hooked"
                )

        pair_result = analyze_pair(
            samples,
            anon_samples,
            warmup_seconds=0.0,
            budget_mib_per_hour=args.budget_mib_per_hour,
            anon_budget_mib_per_hour=anon_budget,
            min_span_seconds=args.min_span_seconds,
            last_pressure_end=None,
            pressure_settle_seconds=0.0,
            measurement_start=measurement_start,
        )
        slope_result = pair_result["rss"]
        anon_result = pair_result["anon"]
        # Report the warm-up from the run clock, not from the first sampled
        # instant (which is a fraction of a second later). This keeps the
        # long-lane >=1800 s contract exact while the slope itself is computed
        # on the selected measurement points with no second warm-up.
        for result in (slope_result, anon_result):
            result["effective_warmup_seconds"] = measurement_start_offset
        if not slope_result.get("ok"):
            violate(f"RSS slope gate failed: {slope_result.get('error')}")
        if not anon_result.get("ok"):
            violate(f"Anon slope gate failed: {anon_result.get('error')}")

        split_result = None
        if args.split_baseline:
            split_result = {
                "rss": analyze_split(
                    samples,
                    warmup_seconds=0.0,
                    budget_mib_per_hour=args.budget_mib_per_hour,
                    min_span_seconds=args.min_span_seconds,
                    last_pressure_end=None,
                    pressure_settle_seconds=0.0,
                    measurement_start=measurement_start,
                    label="RSS",
                ),
                "anon": analyze_split(
                    anon_samples,
                    warmup_seconds=0.0,
                    budget_mib_per_hour=anon_budget,
                    min_span_seconds=args.min_span_seconds,
                    last_pressure_end=None,
                    pressure_settle_seconds=0.0,
                    measurement_start=measurement_start,
                    label="Anon",
                ),
            }
            for name, split in split_result.items():
                split["effective_warmup_seconds"] = measurement_start_offset
                if not split.get("ok"):
                    violate(
                        f"{name} second-half/step baseline failed: "
                        f"{split.get('error')}"
                    )

        sampling_span_seconds = (
            samples[-1][0] - samples[0][0] if len(samples) >= 2 else 0.0
        )
        measurement_rss_samples = [
            point for point in samples if point[0] >= measurement_start
        ]
        measurement_span_seconds = (
            measurement_rss_samples[-1][0] - measurement_rss_samples[0][0]
            if len(measurement_rss_samples) >= 2
            else 0.0
        )
        # The measured window is contiguous: pressure and the clean-load
        # handshake are both before measurement_start, so no sample gap needs
        # to be subtracted from this window.
        active_sampling_seconds = measurement_span_seconds
        expected_samples = (
            int(measurement_span_seconds / args.sample_interval) + 1
            if measurement_rss_samples
            else 0
        )
        measurement_handshake_seconds = (
            measurement_handshake["seconds"]
            if measurement_handshake is not None
            else 0.0
        )

        final = sampler.capture()
        if final.cgroup_events["oom_kill"] > initial.cgroup_events["oom_kill"]:
            violate("OOM kill counter increased during the run")
        if final.cgroup_swap_current_bytes != 0:
            violate("swap in use at end of run")

        report = {
            "schema": "issue34-memory-soak/v1",
            "scope": scope,
            "target": {
                "command": command,
                "kind": args.target_kind,
                "pid": pid,
                "starttime": sampler.starttime,
                "same_pid": final.starttime == sampler.starttime,
            },
            "cgroup": cgroup_ctx.to_dict() if cgroup_ctx else None,
            "duration_seconds": time.monotonic() - started,
            "samples": len(samples),
            "measurement_samples": len(measurement_rss_samples),
            "sampling_span_seconds": sampling_span_seconds,
            "measurement_span_seconds": measurement_span_seconds,
            "pressure_blocked_seconds": pressure_blocked_seconds,
            "measurement_handshake_seconds": measurement_handshake_seconds,
            "measurement_handshake_end_offset_seconds": (
                measurement_handshake_end_offset
            ),
            "measurement_stabilization": measurement_stabilization,
            "measurement_stabilization_seconds": measurement_stabilization_seconds,
            "measurement_stabilization_end_offset_seconds": (
                measurement_stabilization_end_offset
            ),
            "measurement_plateau_rate_mib_per_min": measurement_plateau_rate,
            "measurement_stabilization_plateaued": measurement_plateaued,
            "active_sampling_seconds": active_sampling_seconds,
            "expected_samples": expected_samples,
            "measurement_start_offset_seconds": measurement_start_offset,
            "pressure_end_offset_seconds": pressure_end_offset,
            "pressure_before_measurement": pressure_before_measurement,
            "pre_measurement_load_errors": pre_measurement_load_errors,
            "peak_rss_mib": peak_rss / MIB,
            "max_rss_mib": args.max_rss_mib,
            "screen_only": bool(args.screen_only),
            "screen_note": (
                "SCREEN ONLY: the short lane is a fast screen, not a memory "
                "closure. Only the nightly/dedicated 0.5 MiB/h RSS+Anon gates "
                "may close Issue 34."
                if args.screen_only
                else None
            ),
            "budgets": {
                "rss_mib_per_hour": args.budget_mib_per_hour,
                "anon_mib_per_hour": anon_budget,
                "max_rss_mib": args.max_rss_mib,
            },
            "warmup_seconds": args.warmup_seconds,
            "slope": slope_result,
            "anon_slope": anon_result,
            "split_baseline": split_result,
            "tls_verification": {
                "initial": verify_results[0] if verify_results else None,
                "periodic_count": max(0, len(verify_results) - 1),
                "paused_during_pressure": paused_verifications,
                "errors": verify_errors,
                "contract": "CA verified, ALPN h2, SNI sent, payload echoed",
            },
            "measurement": {
                "start_offset_seconds": measurement_start_offset,
                "span_seconds": measurement_span_seconds,
                "samples": len(measurement_rss_samples),
                "expected_samples": expected_samples,
                "load_errors": measurement_load_errors,
                "requests_ok": measurement_requests_ok,
                "pre_measurement_load_errors": pre_measurement_load_errors,
                "handshake_seconds": measurement_handshake_seconds,
                "handshake_end_offset_seconds": measurement_handshake_end_offset,
                "stabilization_seconds": measurement_stabilization_seconds,
                "stabilization_end_offset_seconds": (
                    measurement_stabilization_end_offset
                ),
                "stabilization_plateaued": measurement_plateaued,
                "plateau_rate_mib_per_min": measurement_plateau_rate,
                "stabilization_window_seconds": (
                    measurement_stabilization["window_seconds"]
                    if measurement_stabilization is not None
                    else STABILIZATION_WINDOW_SECONDS
                ),
                "stabilization_max_rate_mib_per_min": (
                    measurement_stabilization["max_rate_mib_per_min"]
                    if measurement_stabilization is not None
                    else STABILIZATION_MAX_RATE_MIB_PER_MIN
                ),
                "handshake_load_errors": (
                    measurement_handshake["errors"]
                    if measurement_handshake is not None
                    else 0
                ),
                "max_load_errors": args.max_load_errors,
            },
            "load": {
                **load_summary,
                "measurement_errors": measurement_load_errors,
                "pressure_load_errors": pressure_load_errors,
            },
            "pressure": {
                "cycles": args.pressure_cycles,
                "last_pressure_end": last_pressure_end,
                "end_offset_seconds": pressure_end_offset,
                "before_measurement": pressure_before_measurement,
                "pressure_load_errors": pressure_load_errors,
                "result": pressure_summary,
            },
            "allocator_stats": {
                "source": args.stats_source,
                "samples": stats_samples,
                "last": stats_last,
                "required_keys": list(required_keys),
            },
            "node_status": {
                "source": args.status_source,
                "required": args.require_node_status,
                "samples": status_samples,
                "required_fields": list(status_fields),
                "first": status_first,
                "last": status_last,
                "errors": status_errors,
                "coverage_gap": node_status_gap,
                "require_complete_worker_coverage": (
                    args.require_complete_worker_coverage
                ),
                "note": (
                    "controller-reported resourceGovernor snapshot; the RSS gate "
                    "above uses kernel /proc RSS, never mimalloc process info"
                ),
            },
            "final_sample": final.to_dict(),
            "violations": violations,
            "verdict": "pass" if not violations else "fail",
        }
        with open(args.out, "w", encoding="utf-8") as handle:
            json.dump(report, handle, indent=2, sort_keys=True)
        print(json.dumps({"verdict": report["verdict"], "violations": violations}, indent=2))
        return 0 if not violations else 1
    except (
        SoakError,
        procfs.StatError,
        pressure.PressureError,
        loadgen.LoadGenError,
        stats.StatsError,
        status.StatusError,
    ) as exc:
        traceback.print_exc()
        return 2
    finally:
        verifier_stop.set()
        verifier_pause.clear()
        if runner is not None:
            runner.stop()
            runner.join(timeout=10)
        if target is not None and target.poll() is None:
            target.terminate()
            try:
                target.wait(timeout=15)
            except subprocess.TimeoutExpired:
                target.kill()
                target.wait(timeout=10)
        if target_log is not None:
            target_log.close()
        if origin_server is not None:
            origin_server.stop()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target-cmd", required=True)
    parser.add_argument("--target-arg", action="append", default=[], dest="target_args")
    parser.add_argument(
        "--target-kind",
        default="other",
        choices=["bench-defense", "bench-proxy", "full-edge", "harness", "other"],
    )
    parser.add_argument("--allow-bench-proxy-surrogate", action="store_true")
    parser.add_argument("--target-env", action="append", default=[])
    parser.add_argument("--target-pid-file", default=None)
    parser.add_argument("--target-log", default=None,
                        help="redirect target stdout/stderr (L4METRICS is verbose)")

    parser.add_argument("--origin", choices=["auto", "external"], default="auto")
    parser.add_argument("--origin-port", type=int, default=0)
    parser.add_argument("--origin-udp-port", type=int, default=8054)
    parser.add_argument("--origin-payload-bytes", type=int, default=16 * 1024)

    parser.add_argument("--tls-host", default=DEFAULT_TLS_HOST)
    parser.add_argument("--tls-port", type=int, required=True)
    parser.add_argument("--tls-ca", required=True)
    parser.add_argument("--tls-path", default="/issue34/payload")
    parser.add_argument("--tls-verify-interval", type=float, default=60.0)
    parser.add_argument("--ready-timeout-seconds", type=float, default=90.0,
                        dest="ready_timeout_seconds")
    parser.add_argument("--load-concurrency", type=int, default=8)
    parser.add_argument("--load-method", choices=["GET", "POST"], default="POST")
    parser.add_argument("--load-path", default="/issue34/payload")
    parser.add_argument("--load-payload", default=None)
    parser.add_argument("--max-requests", type=int, default=0)
    parser.add_argument("--request-timeout-seconds", type=float, default=10.0)
    parser.add_argument("--min-requests", type=int, default=1)
    parser.add_argument("--max-load-errors", type=int, default=None)

    parser.add_argument("--duration", type=float, required=True, dest="duration_seconds")
    parser.add_argument("--warmup-seconds", type=float, default=120.0)
    parser.add_argument("--min-span-seconds", type=float, default=300.0)
    parser.add_argument("--sample-interval", type=float, default=1.0)
    parser.add_argument("--max-samples", type=int, default=1_000_000)
    parser.add_argument(
        "--measurement-clean-seconds",
        type=float,
        default=MIN_CLEAN_LOAD_SECONDS,
        dest="measurement_clean_seconds",
        help=(
            "required continuous clean-2xx window after pressure before the "
            f"measurement clock starts (minimum {MIN_CLEAN_LOAD_SECONDS:g}s)"
        ),
    )
    parser.add_argument(
        "--measurement-handshake-timeout-seconds",
        type=float,
        default=60.0,
        dest="measurement_handshake_timeout_seconds",
        help="bounded total wait for the post-pressure clean-load handshake",
    )
    parser.add_argument(
        "--measurement-stabilize-timeout-seconds",
        type=float,
        default=STABILIZATION_TIMEOUT_SECONDS,
        dest="measurement_stabilize_timeout_seconds",
        help=(
            "bounded wait after pressure for the trailing RSS rate to flatten "
            f"(<= {STABILIZATION_MAX_RATE_MIB_PER_MIN:g} MiB/min for "
            f"{STABILIZATION_REQUIRED_WINDOWS} consecutive "
            f"{STABILIZATION_WINDOW_SECONDS:g}s windows)"
        ),
    )
    parser.add_argument(
        "--budget-mib-per-hour",
        type=float,
        default=0.5,
        help="RSS upper-bound budget; the strict Issue 34 value is 0.5 MiB/h",
    )
    parser.add_argument(
        "--anon-budget-mib-per-hour",
        type=float,
        default=None,
        help="anonymous-RSS budget; defaults to --budget-mib-per-hour",
    )
    parser.add_argument(
        "--split-baseline",
        action="store_true",
        help="also gate the second half and the half-to-half step (dedicated runs)",
    )
    parser.add_argument(
        "--screen-only",
        action="store_true",
        help="mark the run as a screen; it can never be reported as closure",
    )
    parser.add_argument("--max-rss-mib", type=float, default=2048.0)

    parser.add_argument("--stats-source", default=None)
    parser.add_argument("--require-allocator-stats", action="store_true")
    parser.add_argument("--require-stats-keys", default=None)
    parser.add_argument("--require-stats-samples", type=int, default=0)
    parser.add_argument(
        "--stats-ready-timeout-seconds",
        type=float,
        default=30.0,
        dest="stats_ready_timeout_seconds",
        help="bounded wait for the target's first allocator stats document",
    )

    parser.add_argument("--status-source", default=None,
                        help="node status JSON/JSONL file or URL captured from a fake backend")
    parser.add_argument("--require-node-status", action="store_true")
    parser.add_argument(
        "--require-complete-worker-coverage",
        action="store_true",
        help=(
            "treat coversAllWorkers=false as a violation; without it the gap "
            "is reported but the required node counters are still validated"
        ),
    )
    parser.add_argument("--require-status-fields", default=None)
    parser.add_argument("--require-status-samples", type=int, default=1)
    parser.add_argument("--status-interval", type=float, default=5.0)
    parser.add_argument(
        "--status-ready-timeout-seconds",
        type=float,
        default=STATUS_READY_TIMEOUT_SECONDS,
        dest="status_ready_timeout_seconds",
        help=(
            "bounded startup wait for the controller to proxy the first "
            "complete resourceGovernor snapshot"
        ),
    )

    parser.add_argument("--pressure-cycles", type=int, default=0)
    parser.add_argument("--pressure-start-fraction", type=float, default=0.25)
    parser.add_argument("--pressure-bytes", type=int, default=int(1.5 * GIB))
    parser.add_argument("--pressure-hold-seconds", type=float, default=10.0)
    parser.add_argument("--pressure-settle-seconds", type=float, default=120.0)
    parser.add_argument(
        "--pressure-recovery-probe-seconds",
        type=float,
        default=0.0,
        help="keep sampling after recovery; long runs require >= 30",
    )
    parser.add_argument(
        "--pressure-long-recovery",
        action="store_true",
        help="require >= 10 cycles, >= 60 s hold and a >= 30 s recovery probe",
    )
    parser.add_argument("--pressure-max-bytes", type=int, default=2 * GIB)
    parser.add_argument("--pressure-swap-max", type=int, default=0)
    parser.add_argument("--cgroup-root", default=None)

    parser.add_argument("--samples-jsonl", default=None)
    parser.add_argument("--progress-file", default=None)
    parser.add_argument("--out", required=True)
    return parser


def main(argv=None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if not (0.0 <= args.pressure_start_fraction < 1.0):
        parser.error("--pressure-start-fraction must be in [0, 1)")
    if args.pressure_cycles > 0 and args.pressure_start_fraction * args.duration_seconds < 5:
        parser.error("pressure cycles would start too early; raise --pressure-start-fraction")
    try:
        return run(args)
    except (
        SoakError,
        procfs.StatError,
        pressure.PressureError,
        stats.StatsError,
        status.StatusError,
    ) as exc:
        print(f"issue34 soak: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
