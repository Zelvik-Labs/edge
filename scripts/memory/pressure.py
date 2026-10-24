"""cgroup v2 pressure cycles for the Issue 34 soak.

The soak must run inside a **2 GiB, swap=0** memory cgroup and survive
repeated pressure cycles with the *same PID* (same ``/proc/<pid>/stat``
starttime). This module:

* verifies the ambient cgroup limits (when the workflow runs the soak under
  ``systemd-run -p MemoryMax=2G -p MemorySwapMax=0 -p MemoryHigh=...``);
* optionally creates a cgroup itself when running as root on a dedicated host;
* allocates anonymous memory in cycles, touching every page so cgroup
  accounting (``memory.current`` and ``memory.events``) reflects real pressure.
  The requested ``pressure_bytes`` is an upper bound, not a literal allocation:
  each cycle targets a bounded margin above ``memory.high`` with reserve below
  ``memory.max``. A fixed allocation larger than the headroom above
  ``memory.high`` makes the kernel throttle every fault and the allocator
  livelocks instead of becoming resident;
* emits ``ISSUE34_PRESSURE {json}`` progress lines (also optionally to a file)
  with phases ``cycle_start``, ``high_or_max_entered``, ``release``,
  ``recovered`` and ``resident_timeout`` (with the last cgroup evidence);

Missing cgroup files or counters are hard errors, never zero. A run whose
cycles produced no ``high``/``max`` events fails: the target was not actually
under pressure.
"""

from __future__ import annotations

import json
import mmap
import os
import select
import signal
import sys
import time
from dataclasses import dataclass

from . import procfs

GIB = 1024**3
MIB = 1024**2
PAGE = 4096
# A fixed 1.5 GiB allocation on top of a ~384 MiB baseline exceeds
# memory.high=1.5 GiB by ~384 MiB. Above memory.high the kernel throttles every
# fault and reclaims; with no swap the anonymous pages are unreclaimable, so the
# child stops making progress and the cycle times out. Touch the bulk below high
# and cross it by only a small bounded margin: high + at most 16 MiB, with at
# least 256 MiB below max for the Edge, page cache and fault-in headroom.
PRESSURE_OVER_HIGH_BYTES = 16 * MIB
PRESSURE_RESERVE_BYTES = 256 * MIB
MIN_PRESSURE_BYTES = 64 * MIB
# Resident proof is bounded independently of the hold time. Waiting longer does
# not help an allocation the kernel is throttling; it only delays the failure
# report and keeps ~1 GiB charged to the cgroup.
RESIDENT_TIMEOUT_SECONDS = 120.0
RESIDENT_POLL_SECONDS = 1.0
PROGRESS_PREFIX = "ISSUE34_PRESSURE "


class PressureError(RuntimeError):
    """The cgroup contract or a pressure cycle was not satisfied."""


def _read(path: str) -> str:
    try:
        with open(path, "r", encoding="ascii") as handle:
            return handle.read()
    except OSError as exc:
        raise PressureError(f"cannot read {path}: {exc}") from exc


def _write(path: str, value: str) -> None:
    try:
        with open(path, "w", encoding="ascii") as handle:
            handle.write(value)
    except OSError as exc:
        raise PressureError(f"cannot write {path}: {exc}") from exc


def _int_or_max(raw: str, path: str) -> int | None:
    raw = raw.strip()
    if raw == "max":
        return None
    try:
        value = int(raw, 10)
    except ValueError as exc:
        raise PressureError(f"{path}: not an integer or 'max': {raw!r}") from exc
    if value < 0:
        raise PressureError(f"{path}: negative value {value}")
    return value


def _read_limit(directory: str, name: str) -> int | None:
    return _int_or_max(_read(os.path.join(directory, name)), os.path.join(directory, name))


def _cgroup2_dir(pid: int) -> str:
    text = _read(f"/proc/{pid}/cgroup")
    for line in text.splitlines():
        parts = line.split(":", 2)
        if len(parts) == 3 and parts[0] == "0" and parts[1] == "":
            relative = parts[2].strip()
            if not relative.startswith("/"):
                raise PressureError(f"/proc/{pid}/cgroup: non-absolute {relative!r}")
            return os.path.join(procfs.CGROUP2_ROOT, relative.lstrip("/"))
    raise PressureError("no cgroup v2 (0::) entry; cgroup v1 is unsupported")


@dataclass(frozen=True)
class CgroupContext:
    path: str
    created: bool
    memory_max: int | None
    memory_high: int | None
    swap_max: int | None

    def to_dict(self) -> dict:
        return {
            "path": self.path,
            "created": self.created,
            "memory_max": self.memory_max,
            "memory_high": self.memory_high,
            "swap_max": self.swap_max,
        }


def _enable_memory_controller(directory: str) -> None:
    controllers_path = os.path.join(directory, "cgroup.controllers")
    subtree_path = os.path.join(directory, "cgroup.subtree_control")
    controllers = _read(controllers_path).split()
    if "memory" not in controllers:
        raise PressureError(f"{controllers_path} does not list the memory controller")
    current = _read(subtree_path).split()
    if "memory" not in current:
        _write(subtree_path, "+memory")


def _create_cgroup_under(root: str, memory_max: int, memory_high: int, swap_max: int) -> str:
    path = os.path.join(root, f"issue34-{os.getpid()}")
    try:
        os.makedirs(path, exist_ok=False)
    except OSError as exc:
        raise PressureError(f"cannot create cgroup {path}: {exc}") from exc
    try:
        _write(os.path.join(path, "memory.max"), str(memory_max))
        _write(os.path.join(path, "memory.swap.max"), str(swap_max))
        _write(os.path.join(path, "memory.high"), str(memory_high))
        _write(os.path.join(path, "cgroup.procs"), str(os.getpid()))
    except PressureError:
        try:
            os.rmdir(path)
        except OSError:
            pass
        raise
    return path


def ensure_context(
    *,
    require_memory_max: int | None,
    require_swap_max: int | None,
    require_high: bool,
    cgroup_root: str | None,
) -> CgroupContext:
    """Use the ambient 2 GiB/no-swap cgroup or create one when permitted."""
    ambient = _cgroup2_dir(os.getpid())
    memory_max = _read_limit(ambient, "memory.max")
    memory_high = _read_limit(ambient, "memory.high")
    swap_max = _read_limit(ambient, "memory.swap.max")

    if cgroup_root:
        root = cgroup_root.rstrip("/")
        parent = os.path.dirname(root) or "/sys/fs/cgroup"
        os.makedirs(root, exist_ok=True)
        _enable_memory_controller(parent)
        _enable_memory_controller(root)
        path = _create_cgroup_under(
            root,
            require_memory_max if require_memory_max is not None else 2 * GIB,
            require_memory_max if require_memory_max is not None else 2 * GIB,
            require_swap_max if require_swap_max is not None else 0,
        )
        # memory.high must be below memory.max for events to be meaningful.
        high = _read_limit(path, "memory.high")
        if high is None or high >= (_read_limit(path, "memory.max") or 0):
            _write(os.path.join(path, "memory.high"), str(int(1.5 * GIB)))
        return CgroupContext(
            path=path,
            created=True,
            memory_max=_read_limit(path, "memory.max"),
            memory_high=_read_limit(path, "memory.high"),
            swap_max=_read_limit(path, "memory.swap.max"),
        )

    problems = []
    if require_memory_max is not None and memory_max != require_memory_max:
        problems.append(f"memory.max={memory_max!r}, require {require_memory_max}")
    if require_swap_max is not None and swap_max != require_swap_max:
        problems.append(f"memory.swap.max={swap_max!r}, require {require_swap_max}")
    if require_high and memory_high is None:
        problems.append("memory.high=max; pressure events cannot be generated")
    if (
        require_high
        and memory_max is not None
        and memory_high is not None
        and memory_high >= memory_max
    ):
        problems.append(f"memory.high={memory_high} is not below memory.max={memory_max}")
    if problems:
        raise PressureError(
            "ambient cgroup does not satisfy the Issue 34 contract ("
            + "; ".join(problems)
            + "). Run the soak under: systemd-run --scope --property=MemoryMax=2G "
            "--property=MemoryHigh=1536M --property=MemorySwapMax=0 -- "
            "python3 -I scripts/memory/soak.py ..."
        )
    return CgroupContext(
        path=ambient,
        created=False,
        memory_max=memory_max,
        memory_high=memory_high,
        swap_max=swap_max,
    )


class Progress:
    """Deterministic progress markers consumed by the soak and CI."""

    def __init__(self, file_path: str | None = None, stream=None):
        self.file_path = file_path
        self.stream = stream or sys.stdout
        self.lines: list[dict] = []

    def emit(self, phase: str, **fields) -> dict:
        record = {"phase": phase, "monotonic_s": time.monotonic(), **fields}
        line = PROGRESS_PREFIX + json.dumps(record, sort_keys=True)
        self.lines.append(record)
        print(line, file=self.stream, flush=True)
        if self.file_path:
            try:
                with open(self.file_path, "a", encoding="utf-8") as handle:
                    handle.write(line + "\n")
                    handle.flush()
            except OSError as exc:
                raise PressureError(
                    f"cannot append progress file {self.file_path}: {exc}"
                ) from exc
        return record


def _touch_pages(mapping: mmap.mmap, size: int) -> None:
    view = memoryview(mapping)
    try:
        for offset in range(0, size, PAGE):
            view[offset] = 1
    finally:
        view.release()


def _cycle_pressure_size(
    *,
    current_bytes: int,
    high_bytes: int | None,
    max_bytes: int | None,
    requested_bytes: int,
) -> tuple[int, int]:
    """Return ``(alloc_bytes, target_total_bytes)`` for one pressure cycle.

    ``requested_bytes`` is the operator's upper bound. Allocating it literally
    can be impossible: with ``memory.high`` below ``current + requested``, the
    kernel throttles every fault above high and the child livelocks (observed in
    CI with max=2 GiB, high=1.5 GiB, current=384 MiB, requested=1.5 GiB).
    Instead, target ``high + bounded_overage`` and keep a reserve below
    ``memory.max`` so the Edge, page cache and fault-in path have headroom.
    """
    if requested_bytes < MIN_PRESSURE_BYTES:
        raise PressureError(
            f"pressure_bytes must be at least {MIN_PRESSURE_BYTES // MIB} MiB, "
            f"got {requested_bytes}"
        )
    if max_bytes is None:
        return requested_bytes, current_bytes + requested_bytes
    if max_bytes <= 0:
        raise PressureError(f"memory.max must be positive, got {max_bytes}")
    if high_bytes is None:
        raise PressureError(
            "memory.high is 'max'; a high/max event cannot be generated"
        )
    high = min(high_bytes, max_bytes)
    gap = max(0, max_bytes - high)
    reserve = min(PRESSURE_RESERVE_BYTES, max(MIN_PRESSURE_BYTES, gap // 2))
    ceiling = max_bytes - reserve
    if ceiling < high:
        raise PressureError(
            "memory.high is too close to memory.max for a genuine pressure "
            f"cycle: high={high} max={max_bytes} reserve={reserve}; leave at "
            f"least {MIN_PRESSURE_BYTES} bytes between them"
        )
    over_high = min(
        PRESSURE_OVER_HIGH_BYTES,
        max(PRESSURE_OVER_HIGH_BYTES // 2, gap // 8),
    )
    target = min(
        ceiling,
        max(high + over_high, current_bytes + MIN_PRESSURE_BYTES),
    )
    required = max(MIN_PRESSURE_BYTES, target - current_bytes)
    if requested_bytes < required:
        raise PressureError(
            f"requested pressure {requested_bytes} is below the {required} bytes "
            f"needed to reach the pressure target {target} from current="
            f"{current_bytes} (high={high}); increase --pressure-bytes"
        )
    alloc = min(requested_bytes, target - current_bytes)
    if alloc < MIN_PRESSURE_BYTES:
        raise PressureError(
            "cgroup headroom is too small for a genuine pressure cycle: "
            f"current={current_bytes} high={high} max={max_bytes} "
            f"target={target} alloc={alloc}; need at least "
            f"{MIN_PRESSURE_BYTES} bytes below max"
        )
    return alloc, target


def _spawn_allocator(size: int) -> tuple[int, int, int]:
    """Fork a child that faults ``size`` private anonymous bytes and holds them.

    Returns ``(pid, ready_fd, release_fd)``. Explicit
    ``MAP_PRIVATE|MAP_ANONYMOUS`` matters: ``mmap.mmap(-1, size)`` defaults to
    ``MAP_SHARED`` (shmem), which the kernel can reclaim without swap, so it
    does not prove resident anonymous memory. The child writes one byte on
    ``ready_fd`` once every page is resident, then blocks until the parent
    closes ``release_fd`` (EOF), unmaps and exits.
    """
    ready_r, ready_w = os.pipe()
    release_r, release_w = os.pipe()
    pid = os.fork()
    if pid == 0:  # pragma: no cover - exercised by the soak, not unit tests
        try:
            os.close(ready_r)
            os.close(release_w)
            anonymous = getattr(mmap, "MAP_ANONYMOUS", None) or getattr(
                mmap, "MAP_ANON"
            )
            mapping = mmap.mmap(
                -1,
                size,
                flags=mmap.MAP_PRIVATE | anonymous,
                prot=mmap.PROT_READ | mmap.PROT_WRITE,
            )
            _touch_pages(mapping, size)
            os.write(ready_w, b"A")
            os.close(ready_w)
            while os.read(release_r, 1):
                pass
            mapping.close()
            os.close(release_r)
            os._exit(0)
        except BaseException:
            os._exit(2)
    os.close(ready_w)
    os.close(release_r)
    return pid, ready_r, release_w


def _wait_ready(
    read_fd: int,
    timeout: float,
    *,
    sampler: procfs.Sampler | None = None,
    cycle: int | None = None,
    allocated_bytes: int | None = None,
    target_bytes: int | None = None,
    progress: Progress | None = None,
    poll_interval: float = RESIDENT_POLL_SECONDS,
) -> None:
    """Wait for the allocator's resident proof, bounded independently of hold.

    The wait is not extended by ``hold_seconds``: if the kernel is throttling
    faults above ``memory.high``, more time does not make the pages resident.
    We sample the cgroup while waiting and report the last current/peak/high/
    max/swap/events as evidence on timeout instead of only an opaque error.
    """
    if sampler is None or target_bytes is None:
        ready, _, _ = select.select([read_fd], [], [], timeout)
        if not ready:
            raise PressureError(
                "pressure allocator did not make its pages resident in time"
            )
        data = os.read(read_fd, 1)
        if data != b"A":
            raise PressureError("pressure allocator failed before signalling readiness")
        return

    started = time.monotonic()
    last = None
    peak = None
    while True:
        now = time.monotonic()
        remaining = timeout - (now - started)
        if remaining <= 0:
            detail = ""
            if last is not None:
                evidence = {
                    "elapsed_seconds": round(now - started, 1),
                    "allocated_bytes": allocated_bytes,
                    "target_bytes": target_bytes,
                    "memory_current": last.cgroup_current_bytes,
                    "peak_current": peak,
                    "memory_high": last.cgroup_high_bytes,
                    "memory_max": last.cgroup_max_bytes,
                    "swap_current": last.cgroup_swap_current_bytes,
                    "events": last.cgroup_events,
                }
                if progress is not None:
                    progress.emit("resident_timeout", cycle=cycle, **evidence)
                detail = (
                    f"; current={evidence['memory_current']} "
                    f"peak={evidence['peak_current']} target={target_bytes} "
                    f"high={evidence['memory_high']} max={evidence['memory_max']} "
                    f"swap={evidence['swap_current']} events={evidence['events']}"
                )
            raise PressureError(
                "pressure allocator did not make its pages resident within "
                f"{timeout:g}s{detail}"
            )
        ready, _, _ = select.select([read_fd], [], [], min(poll_interval, remaining))
        if ready:
            data = os.read(read_fd, 1)
            if data != b"A":
                raise PressureError(
                    "pressure allocator failed before signalling readiness"
                )
            return
        last = sampler.capture()
        peak = max(peak or 0, last.cgroup_current_bytes)


def _release_and_reap(
    pid: int,
    release_fd: int,
    timeout: float = 60.0,
    *,
    kill_first: bool = False,
) -> None:
    """Release the allocator, or kill it immediately on a failed cycle.

    ``kill_first`` is used when the cycle already failed (for example the
    resident timeout): a child stuck in cgroup reclaim may never observe the
    pipe EOF, and waiting the normal timeout would keep the allocation charged
    and hide the original error behind a cleanup error.
    """
    try:
        os.close(release_fd)
    except OSError:
        pass
    if kill_first:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + timeout
    while True:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited == pid:
            code = os.waitstatus_to_exitcode(status)
            if code != 0 and not kill_first:
                raise PressureError(f"pressure allocator exited with status {code}")
            return
        if time.monotonic() >= deadline:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
            raise PressureError("pressure allocator did not exit in time")
        time.sleep(0.05)


def _pressure_entered(baseline_events: dict, sample) -> bool:
    high_delta = sample.cgroup_events["high"] - baseline_events["high"]
    max_delta = sample.cgroup_events["max"] - baseline_events["max"]
    if high_delta > 0 or max_delta > 0:
        return True
    if sample.cgroup_high_bytes is not None:
        return sample.cgroup_current_bytes >= sample.cgroup_high_bytes
    return False


LONG_RECOVERY_MIN_CYCLES = 10
LONG_RECOVERY_MIN_HOLD_SECONDS = 60.0
LONG_RECOVERY_MIN_PROBE_SECONDS = 30.0


def _probe_sustained_recovery(
    sampler: procfs.Sampler,
    *,
    ceiling_bytes: int,
    seconds: float,
    poll_interval: float,
    progress: Progress,
    cycle: int,
) -> int:
    """Keep sampling after recovery so a transient dip cannot count as recovery.

    A single below-ceiling sample can be a momentary dip caused by allocator
    return paths; the long-run contract requires the cgroup to *stay* below
    the ceiling (and swap to stay at zero) for a sustained window.
    """
    if seconds <= 0:
        return 0
    deadline = time.monotonic() + seconds
    samples = 0
    while time.monotonic() < deadline:
        candidate = sampler.capture()
        samples += 1
        sampler.verify_identity()
        if candidate.cgroup_current_bytes > ceiling_bytes:
            raise PressureError(
                f"cycle {cycle}: recovery was not sustained after {samples} "
                f"probe samples; cgroup memory.current "
                f"{candidate.cgroup_current_bytes} > {ceiling_bytes}"
            )
        if candidate.cgroup_swap_current_bytes != 0:
            raise PressureError(
                f"cycle {cycle}: swap in use during the recovery probe "
                f"({candidate.cgroup_swap_current_bytes} bytes)"
            )
        time.sleep(poll_interval)
    progress.emit(
        "recovery_probe",
        cycle=cycle,
        samples=samples,
        seconds=seconds,
        ceiling_bytes=ceiling_bytes,
    )
    return samples


def run_cycles(
    sampler: procfs.Sampler,
    *,
    cycles: int,
    pressure_bytes: int,
    hold_seconds: float,
    settle_timeout_seconds: float,
    progress: Progress,
    require_events: bool = True,
    poll_interval: float = 0.2,
    recovery_slack_bytes: int = 64 * MIB,
    recovery_probe_seconds: float = 0.0,
    long_recovery: bool = False,
) -> dict:
    """Run deterministic pressure cycles and verify same-PID recovery."""
    if cycles <= 0:
        raise PressureError("cycles must be positive")
    if pressure_bytes < MIN_PRESSURE_BYTES:
        raise PressureError(
            f"pressure_bytes must be at least {MIN_PRESSURE_BYTES // MIB} MiB"
        )
    if long_recovery:
        # A 5 s / one-shot cycle proves the code path, not recovery. Long runs
        # must hold real pressure for a minute and verify recovery afterwards.
        if cycles < LONG_RECOVERY_MIN_CYCLES:
            raise PressureError(
                f"long recovery requires at least {LONG_RECOVERY_MIN_CYCLES} "
                f"pressure cycles, got {cycles}"
            )
        if hold_seconds < LONG_RECOVERY_MIN_HOLD_SECONDS:
            raise PressureError(
                f"long recovery requires hold_seconds >= "
                f"{LONG_RECOVERY_MIN_HOLD_SECONDS:.0f}, got {hold_seconds}"
            )
        if recovery_probe_seconds < LONG_RECOVERY_MIN_PROBE_SECONDS:
            raise PressureError(
                f"long recovery requires recovery_probe_seconds >= "
                f"{LONG_RECOVERY_MIN_PROBE_SECONDS:.0f}, got "
                f"{recovery_probe_seconds}"
            )
    baseline = sampler.capture()
    baseline_events = dict(baseline.cgroup_events)
    # Recovery is judged against a stable pressure-block baseline, not the
    # transient current immediately after the previous cycle. In CI run
    # 40094513096 the second cycle's pre-pressure current was ~15 MiB below the
    # first cycle's steady state, so its per-cycle ceiling was too low and the
    # 10 s probe failed by 528,384 bytes while the process was recovering
    # normally. The baseline may rise if a later pre-cycle sample is higher,
    # but the slack stays exactly the configured 64 MiB.
    recovery_baseline_bytes = baseline.cgroup_current_bytes
    records = []
    probe_samples_total = 0
    for index in range(1, cycles + 1):
        before = sampler.capture()
        recovery_baseline_bytes = max(
            recovery_baseline_bytes, before.cgroup_current_bytes
        )
        # Per-cycle baseline: the run-level counters already moved in earlier
        # cycles, so using them would let later cycles "pass" before any new
        # pressure was accounted.
        cycle_events = dict(before.cgroup_events)
        sampler.verify_identity()
        allocated_bytes, target_bytes = _cycle_pressure_size(
            current_bytes=before.cgroup_current_bytes,
            high_bytes=before.cgroup_high_bytes,
            max_bytes=before.cgroup_max_bytes,
            requested_bytes=pressure_bytes,
        )
        progress.emit(
            "cycle_start",
            cycle=index,
            cycles=cycles,
            pid=sampler.pid,
            starttime=sampler.starttime,
            memory_current=before.cgroup_current_bytes,
            memory_max=before.cgroup_max_bytes,
            memory_high=before.cgroup_high_bytes,
            swap_current=before.cgroup_swap_current_bytes,
            events=before.cgroup_events,
            pressure_bytes=pressure_bytes,
            allocated_bytes=allocated_bytes,
            target_bytes=target_bytes,
        )
        pid, ready_fd, release_fd = _spawn_allocator(allocated_bytes)
        entered_sample = None
        last_candidate = None
        cycle_error = None
        try:
            _wait_ready(
                ready_fd,
                timeout=RESIDENT_TIMEOUT_SECONDS,
                sampler=sampler,
                cycle=index,
                allocated_bytes=allocated_bytes,
                target_bytes=target_bytes,
                progress=progress,
            )
            deadline = time.monotonic() + max(30.0, hold_seconds)
            while time.monotonic() < deadline:
                candidate = sampler.capture()
                last_candidate = candidate
                if _pressure_entered(cycle_events, candidate):
                    entered_sample = candidate
                    break
                time.sleep(poll_interval)
            if entered_sample is None:
                seen = last_candidate or before
                raise PressureError(
                    f"cycle {index}: cgroup never reached memory.high or emitted "
                    f"a high/max event; current={seen.cgroup_current_bytes} "
                    f"before={before.cgroup_current_bytes} target={target_bytes} "
                    f"high={seen.cgroup_high_bytes} events={seen.cgroup_events}"
                )
            progress.emit(
                "high_or_max_entered",
                cycle=index,
                pid=sampler.pid,
                starttime=sampler.starttime,
                memory_current=entered_sample.cgroup_current_bytes,
                memory_high=entered_sample.cgroup_high_bytes,
                memory_max=entered_sample.cgroup_max_bytes,
                events=entered_sample.cgroup_events,
            )
            time.sleep(hold_seconds)
        except BaseException as exc:
            cycle_error = exc
            raise
        finally:
            try:
                os.close(ready_fd)
            except OSError:
                pass
            try:
                _release_and_reap(
                    pid,
                    release_fd,
                    timeout=10.0 if cycle_error is not None else 60.0,
                    kill_first=cycle_error is not None,
                )
            except PressureError:
                # A cleanup failure must not mask the primary pressure error.
                if cycle_error is None:
                    raise

        progress.emit(
            "release",
            cycle=index,
            pid=sampler.pid,
            starttime=sampler.starttime,
        )
        recovered = None
        deadline = time.monotonic() + settle_timeout_seconds
        target_ceiling = recovery_baseline_bytes + recovery_slack_bytes
        while time.monotonic() < deadline:
            candidate = sampler.capture()
            if candidate.cgroup_current_bytes <= target_ceiling:
                recovered = candidate
                break
            time.sleep(poll_interval)
        if recovered is None:
            raise PressureError(
                f"cycle {index}: cgroup memory.current did not recover below "
                f"{target_ceiling} within {settle_timeout_seconds}s"
            )
        sampler.verify_identity()
        if recovered.cgroup_swap_current_bytes != 0:
            raise PressureError(
                f"cycle {index}: swap in use after recovery "
                f"({recovered.cgroup_swap_current_bytes} bytes)"
            )
        progress.emit(
            "recovered",
            cycle=index,
            same_pid=True,
            pid=sampler.pid,
            starttime=sampler.starttime,
            rss_bytes=recovered.rss_bytes,
            memory_current=recovered.cgroup_current_bytes,
            events=recovered.cgroup_events,
        )
        probe_samples = _probe_sustained_recovery(
            sampler,
            ceiling_bytes=target_ceiling,
            seconds=recovery_probe_seconds,
            poll_interval=poll_interval,
            progress=progress,
            cycle=index,
        )
        probe_samples_total += probe_samples
        sampler.verify_identity()
        records.append(
            {
                "cycle": index,
                "requested_bytes": pressure_bytes,
                "allocated_bytes": allocated_bytes,
                "target_bytes": target_bytes,
                "before": before.to_dict(),
                "entered": entered_sample.to_dict(),
                "recovered": recovered.to_dict(),
                "recovery_baseline_bytes": recovery_baseline_bytes,
                "recovery_ceiling_bytes": target_ceiling,
                "recovery_slack_bytes": recovery_slack_bytes,
                "recovery_probe_samples": probe_samples,
            }
        )

    final = sampler.capture()
    delta = {
        key: final.cgroup_events[key] - baseline_events[key]
        for key in ("low", "high", "max", "oom", "oom_kill")
    }
    if require_events and delta["high"] + delta["max"] <= 0:
        raise PressureError(
            "pressure cycles completed without high/max cgroup events: "
            f"deltas={delta}. The target was not under real pressure."
        )
    if delta["oom_kill"] > 0:
        raise PressureError(f"OOM kill occurred during pressure cycles: deltas={delta}")
    if final.cgroup_swap_current_bytes != 0:
        raise PressureError(
            f"swap in use at end of cycles: {final.cgroup_swap_current_bytes} bytes"
        )
    return {
        "cycles": cycles,
        "pressure_bytes": pressure_bytes,
        "allocated_bytes": [record["allocated_bytes"] for record in records],
        "target_bytes": [record["target_bytes"] for record in records],
        "hold_seconds": hold_seconds,
        "settle_timeout_seconds": settle_timeout_seconds,
        "recovery_probe_seconds": recovery_probe_seconds,
        "recovery_probe_samples": probe_samples_total,
        "initial_baseline_current_bytes": baseline.cgroup_current_bytes,
        "recovery_baseline_current_bytes": recovery_baseline_bytes,
        "recovery_slack_bytes": recovery_slack_bytes,
        "long_recovery": long_recovery,
        "event_deltas": delta,
        "same_pid": final.starttime == sampler.starttime,
        "final": final.to_dict(),
        "records": records,
    }
