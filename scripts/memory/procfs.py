"""Strict ``/proc`` and cgroup v2 sampling for the Issue 34 soak.

Design rules
------------
* A missing file, a missing required field or a malformed value is a hard
  error (:class:`StatError`). Counters that legitimately equal zero (for
  example ``oom_kill 0``) are returned as zero; absence is never zero.
* The sampled PID identity is ``/proc/<pid>/stat`` field 22 (``starttime``).
  Every sample re-checks it so a recycled PID cannot be mistaken for the
  process under test.
* Sampling is streaming and bounded. ``/proc/<pid>/maps`` is summarised while
  reading; it is never retained as a list of mappings.
"""

from __future__ import annotations

import os
import time
from dataclasses import asdict, dataclass, field

PROC = "/proc"
CGROUP2_ROOT = os.environ.get("ISSUE34_CGROUP_ROOT", "/sys/fs/cgroup")

_REQUIRED_STATUS_KIB = ("VmRSS", "RssAnon", "VmSwap")
_REQUIRED_ROLLUP_KIB = ("Rss", "Pss", "Pss_Anon", "Anonymous", "AnonHugePages", "Swap")
_REQUIRED_CGROUP_EVENTS = ("low", "high", "max", "oom", "oom_kill")
_REQUIRED_CGROUP_STAT = ("anon", "file", "kernel", "shmem")


class StatError(RuntimeError):
    """A required Linux statistic is unavailable or malformed."""


class PidReuseError(StatError):
    """The PID now belongs to a different process (starttime changed)."""


def _read_text(path: str) -> str:
    try:
        with open(path, "r", encoding="ascii", errors="strict") as handle:
            return handle.read()
    except OSError as exc:
        raise StatError(f"cannot read {path}: {exc}") from exc
    except UnicodeError as exc:  # pragma: no cover - kernel data is ASCII
        raise StatError(f"cannot decode {path}: {exc}") from exc


def _as_int(value: str, source: str, key: str) -> int:
    try:
        parsed = int(value, 10)
    except (TypeError, ValueError) as exc:
        raise StatError(f"{source}: {key} is not an integer: {value!r}") from exc
    if parsed < 0:
        raise StatError(f"{source}: {key} is negative: {parsed}")
    return parsed


def _as_kib(value: str, source: str, key: str) -> int:
    """Parse a ``/proc`` value such as ``1234 kB`` into bytes."""
    parts = value.split()
    if not parts:
        raise StatError(f"{source}: {key} has no value")
    kib = _as_int(parts[0], source, key)
    if len(parts) > 1 and parts[1] != "kB":
        raise StatError(f"{source}: {key} has unexpected unit {parts[1]!r}")
    return kib * 1024


def parse_proc_status(text: str, source: str = "/proc/self/status") -> dict:
    """Parse the required fields from ``/proc/<pid>/status``.

    ``VmSwap`` may be zero (no swap in use) but the key must be present, which
    distinguishes "zero swap" from "this kernel does not expose the field".
    """
    values: dict[str, int] = {}
    for line in text.splitlines():
        key, sep, rest = line.partition(":")
        if not sep:
            continue
        key = key.strip()
        if key in _REQUIRED_STATUS_KIB:
            values[key] = _as_kib(rest, source, key)
        elif key == "Threads":
            values[key] = _as_int(rest.strip(), source, key)
    missing = [key for key in (*_REQUIRED_STATUS_KIB, "Threads") if key not in values]
    if missing:
        raise StatError(f"{source}: missing required fields {missing}")
    return {
        "rss_bytes": values["VmRSS"],
        "anon_bytes": values["RssAnon"],
        "swap_bytes": values["VmSwap"],
        "threads": values["Threads"],
    }


def parse_smaps_rollup(text: str, source: str = "/proc/self/smaps_rollup") -> dict:
    """Parse the required fields from ``/proc/<pid>/smaps_rollup``."""
    values: dict[str, int] = {}
    for line in text.splitlines():
        key, sep, rest = line.partition(":")
        if not sep:
            continue
        key = key.strip()
        if key in _REQUIRED_ROLLUP_KIB:
            values[key] = _as_kib(rest, source, key)
    missing = [key for key in _REQUIRED_ROLLUP_KIB if key not in values]
    if missing:
        raise StatError(f"{source}: missing required fields {missing}")
    return {
        "rollup_rss_bytes": values["Rss"],
        "pss_bytes": values["Pss"],
        "pss_anon_bytes": values["Pss_Anon"],
        "rollup_anon_bytes": values["Anonymous"],
        "anon_huge_bytes": values["AnonHugePages"],
        "rollup_swap_bytes": values["Swap"],
    }


def parse_proc_stat(text: str, source: str = "/proc/self/stat") -> dict:
    """Parse ``/proc/<pid>/stat`` field 22 (``starttime``) safely.

    ``comm`` (field 2) may contain spaces and parentheses, so split after the
    last ``)`` instead of whitespace-splitting the whole line.
    """
    right = text.rfind(")")
    if right < 0:
        raise StatError(f"{source}: malformed stat line (no closing parenthesis)")
    rest = text[right + 1 :].split()
    # ``rest[0]`` is field 3 (state); field 22 is therefore index 19.
    if len(rest) <= 19:
        raise StatError(f"{source}: stat line has {len(rest) + 2} fields, need 22")
    return {"starttime": _as_int(rest[19], source, "starttime")}


def parse_cgroup_events(text: str, source: str = "memory.events") -> dict:
    values: dict[str, int] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) != 2:
            if line.strip():
                raise StatError(f"{source}: malformed line {line!r}")
            continue
        values[parts[0]] = _as_int(parts[1], source, parts[0])
    missing = [key for key in _REQUIRED_CGROUP_EVENTS if key not in values]
    if missing:
        raise StatError(f"{source}: missing required counters {missing}")
    return values


def parse_cgroup_stat(text: str, source: str = "memory.stat") -> dict:
    values: dict[str, int] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) != 2:
            if line.strip():
                raise StatError(f"{source}: malformed line {line!r}")
            continue
        values[parts[0]] = _as_int(parts[1], source, parts[0])
    missing = [key for key in _REQUIRED_CGROUP_STAT if key not in values]
    if missing:
        raise StatError(f"{source}: missing required fields {missing}")
    return values


def parse_pressure(text: str, source: str = "memory.pressure") -> dict:
    """Parse ``some``/``full`` PSI lines into avg10/avg60/avg300/total (µs)."""
    result: dict[str, dict[str, float]] = {}
    for line in text.splitlines():
        parts = line.split()
        if not parts:
            continue
        kind = parts[0]
        if kind not in ("some", "full"):
            raise StatError(f"{source}: unexpected PSI kind {kind!r}")
        fields: dict[str, float] = {}
        for item in parts[1:]:
            key, sep, value = item.partition("=")
            if not sep:
                raise StatError(f"{source}: malformed PSI field {item!r}")
            try:
                fields[key] = float(value)
            except ValueError as exc:
                raise StatError(f"{source}: non-numeric PSI field {item!r}") from exc
        for key in ("avg10", "avg60", "avg300", "total"):
            if key not in fields:
                raise StatError(f"{source}: {kind} missing {key}")
        result[kind] = fields
    if "some" not in result:
        raise StatError(f"{source}: missing 'some' line")
    return result


def read_proc_stat(pid: int) -> dict:
    return parse_proc_stat(_read_text(f"{PROC}/{pid}/stat"), f"/proc/{pid}/stat")


def read_status(pid: int) -> dict:
    return parse_proc_status(_read_text(f"{PROC}/{pid}/status"), f"/proc/{pid}/status")


def read_smaps_rollup(pid: int) -> dict:
    path = f"{PROC}/{pid}/smaps_rollup"
    return parse_smaps_rollup(_read_text(path), path)


def count_fds(pid: int) -> int:
    path = f"{PROC}/{pid}/fd"
    try:
        return len(os.listdir(path))
    except OSError as exc:
        raise StatError(f"cannot list {path}: {exc}") from exc


def read_max_open_files(pid: int) -> dict:
    path = f"{PROC}/{pid}/limits"
    for line in _read_text(path).splitlines():
        if not line.startswith("Max open files"):
            continue
        parts = line.split()
        # Max open files  <soft>  <hard>  files
        if len(parts) < 5:
            raise StatError(f"{path}: malformed Max open files line {line!r}")
        return {
            "max_open_files_soft": _as_int(parts[3], path, "soft"),
            "max_open_files_hard": _as_int(parts[4], path, "hard"),
        }
    raise StatError(f"{path}: Max open files line is missing")


def read_maps_summary(pid: int, max_lines: int = 5_000_000) -> dict:
    """Stream ``/proc/<pid>/maps`` into bounded counters.

    ``max_lines`` is a guard against an attacker-controlled mapping explosion:
    exceeding it is an error, not a silently truncated summary.
    """
    path = f"{PROC}/{pid}/maps"
    mappings = 0
    mapped_bytes = 0
    anon_bytes = 0
    heap_bytes = 0
    try:
        with open(path, "r", encoding="ascii", errors="strict") as handle:
            for line in handle:
                mappings += 1
                if mappings > max_lines:
                    raise StatError(f"{path}: more than {max_lines} mappings")
                parts = line.split(maxsplit=5)
                if len(parts) < 5:
                    raise StatError(f"{path}: malformed mapping {line!r}")
                start_s, end_s = parts[0].split("-", 1)
                start = int(start_s, 16)
                end = int(end_s, 16)
                if end < start:
                    raise StatError(f"{path}: inverted mapping {line!r}")
                size = end - start
                mapped_bytes += size
                tail = parts[5].strip() if len(parts) > 5 else ""
                if not tail:
                    anon_bytes += size
                elif tail == "[heap]":
                    heap_bytes += size
    except OSError as exc:
        raise StatError(f"cannot read {path}: {exc}") from exc
    except ValueError as exc:
        raise StatError(f"{path}: non-hex mapping range: {exc}") from exc
    return {
        "maps_count": mappings,
        "maps_bytes": mapped_bytes,
        "maps_anon_bytes": anon_bytes,
        "maps_heap_bytes": heap_bytes,
    }


def _cgroup2_dir(pid: int) -> str:
    """Resolve the cgroup v2 directory for ``pid``.

    Only the unified hierarchy (``0::/path``) is supported. A cgroup v1 host is
    an explicit error because the required ``memory.events`` semantics differ.
    """
    text = _read_text(f"{PROC}/{pid}/cgroup")
    for line in text.splitlines():
        parts = line.split(":", 2)
        if len(parts) != 3:
            raise StatError(f"/proc/{pid}/cgroup: malformed line {line!r}")
        if parts[0] == "0" and parts[1] == "":
            relative = parts[2].strip()
            if not relative.startswith("/"):
                raise StatError(f"/proc/{pid}/cgroup: non-absolute path {relative!r}")
            return os.path.join(CGROUP2_ROOT, relative.lstrip("/"))
    raise StatError(
        f"/proc/{pid}/cgroup: no cgroup v2 (0::) entry; cgroup v1 is unsupported"
    )


def _read_cgroup_int(directory: str, name: str, allow_max: bool = False) -> int | None:
    path = os.path.join(directory, name)
    raw = _read_text(path).strip()
    if allow_max and raw == "max":
        return None
    return _as_int(raw, path, name)


def read_cgroup(pid: int) -> dict:
    directory = _cgroup2_dir(pid)
    events_path = os.path.join(directory, "memory.events")
    stat_path = os.path.join(directory, "memory.stat")
    pressure_path = os.path.join(directory, "memory.pressure")
    result = {
        "cgroup_path": directory,
        "cgroup_current_bytes": _read_cgroup_int(directory, "memory.current"),
        "cgroup_max_bytes": _read_cgroup_int(directory, "memory.max", allow_max=True),
        "cgroup_high_bytes": _read_cgroup_int(directory, "memory.high", allow_max=True),
        "cgroup_swap_current_bytes": _read_cgroup_int(directory, "memory.swap.current"),
        "cgroup_swap_max_bytes": _read_cgroup_int(directory, "memory.swap.max", allow_max=True),
        "cgroup_events": parse_cgroup_events(_read_text(events_path), events_path),
        "cgroup_stat": parse_cgroup_stat(_read_text(stat_path), stat_path),
        "cgroup_pressure": parse_pressure(_read_text(pressure_path), pressure_path),
    }
    # Flatten the event counters used by verdicts so their presence is obvious.
    for key, value in result["cgroup_events"].items():
        result[f"cgroup_event_{key}"] = value
    return result


@dataclass(frozen=True)
class Sample:
    wall_time_ns: int
    monotonic_s: float
    pid: int
    starttime: int
    rss_bytes: int
    anon_bytes: int
    pss_bytes: int
    pss_anon_bytes: int
    swap_bytes: int
    anon_huge_bytes: int
    threads: int
    fds: int
    max_open_files_soft: int
    max_open_files_hard: int
    maps_count: int
    maps_bytes: int
    maps_anon_bytes: int
    maps_heap_bytes: int
    cgroup_path: str
    cgroup_current_bytes: int
    cgroup_max_bytes: int | None
    cgroup_high_bytes: int | None
    cgroup_swap_current_bytes: int
    cgroup_swap_max_bytes: int | None
    cgroup_events: dict = field(default_factory=dict)
    cgroup_stat: dict = field(default_factory=dict)
    cgroup_pressure: dict = field(default_factory=dict)

    def to_dict(self) -> dict:
        return asdict(self)


class Sampler:
    """Captures samples while proving the PID still belongs to the same process."""

    def __init__(self, pid: int, expected_starttime: int | None = None):
        if pid <= 0:
            raise StatError(f"invalid pid {pid}")
        self.pid = pid
        self._starttime = expected_starttime
        if self._starttime is None:
            self._starttime = read_proc_stat(pid)["starttime"]

    @property
    def starttime(self) -> int:
        return self._starttime

    def verify_identity(self) -> None:
        try:
            current = read_proc_stat(self.pid)["starttime"]
        except StatError as exc:
            raise StatError(f"process {self.pid} disappeared: {exc}") from exc
        if current != self._starttime:
            raise PidReuseError(
                f"pid {self.pid} was reused: starttime {self._starttime} -> {current}"
            )

    def capture(self) -> Sample:
        self.verify_identity()
        status = read_status(self.pid)
        rollup = read_smaps_rollup(self.pid)
        limits = read_max_open_files(self.pid)
        maps = read_maps_summary(self.pid)
        cgroup = read_cgroup(self.pid)
        return Sample(
            wall_time_ns=time.time_ns(),
            monotonic_s=time.monotonic(),
            pid=self.pid,
            starttime=self._starttime,
            rss_bytes=status["rss_bytes"],
            anon_bytes=status["anon_bytes"],
            pss_bytes=rollup["pss_bytes"],
            pss_anon_bytes=rollup["pss_anon_bytes"],
            swap_bytes=status["swap_bytes"],
            anon_huge_bytes=rollup["anon_huge_bytes"],
            threads=status["threads"],
            fds=count_fds(self.pid),
            max_open_files_soft=limits["max_open_files_soft"],
            max_open_files_hard=limits["max_open_files_hard"],
            maps_count=maps["maps_count"],
            maps_bytes=maps["maps_bytes"],
            maps_anon_bytes=maps["maps_anon_bytes"],
            maps_heap_bytes=maps["maps_heap_bytes"],
            **cgroup,
        )


def read_pid_file(path: str) -> int:
    raw = _read_text(path).strip()
    try:
        pid = int(raw, 10)
    except ValueError as exc:
        raise StatError(f"{path}: invalid pid {raw!r}") from exc
    if pid <= 0:
        raise StatError(f"{path}: invalid pid {pid}")
    return pid
