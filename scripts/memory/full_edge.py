#!/usr/bin/env python3
"""Real production-startup fixture for the Issue 34 full-Edge memory lane.

The Rust example ``examples/issue34-controller.rs`` is the fixture backend. It
serves ControlChannel activation, an HMAC-signed NodeControl snapshot, and a
node-status endpoint while the *real* ``Zelvik-Edge`` binary runs its normal
startup path. This module only owns process lifecycle, prerequisite checks,
and the structured contract consumed by ``scripts/memory/run_soak.py``.

Nothing here is a surrogate:

* ``smoke`` spawns the real Edge, waits for the real TLS listener, and requires
  HTTP/1.1, HTTP/2, CA verification, SNI, payload echo, NodeControl health, and
  telemetry rejection before it returns ``pass``.
* ``serve`` starts only the fake control plane and stays alive; the caller
  starts the real Edge through ``run_soak.py``. Its published ``status`` is
  ``controller-ready``, never ``edge-ready`` until a real health RPC arrives.
* ``soak`` is a thin lifecycle wrapper around the existing
  ``scripts/memory/run_soak.py``. It does not implement a second measurement
  runner.

Exit codes: 0 pass, 1 fixture/soak failure, 2 usage, 78 prerequisites missing
(``EX_CONFIG``), 130 interrupted.

Examples::

    python3 -I scripts/memory/full_edge.py preflight \
        --edge-binary target/debug/Zelvik-Edge

    python3 -I scripts/memory/full_edge.py smoke \
        --edge-binary target/debug/Zelvik-Edge \
        --report "$RUNNER_TEMP/full-edge-smoke.json"

    python3 -I scripts/memory/full_edge.py serve \
        --edge-binary target/debug/Zelvik-Edge \
        --ready-file /tmp/issue34/controller-ready.json \
        --contract-file /tmp/issue34/contract.json

    python3 -I scripts/memory/full_edge.py soak \
        --edge-binary target/debug/Zelvik-Edge \
        --duration 3600 --out /tmp/issue34/soak.json \
        --telemetry-mode unavailable

    # Nightly posture: retryable telemetry for the whole window, then a
    # durable success phase before the run ends. The controller reads the
    # mode file on every upload, so the Edge PID never changes.
    python3 -I scripts/memory/full_edge.py soak \
        --edge-binary target/debug/Zelvik-Edge \
        --duration 14400 --recovery-seconds 60 \
        --telemetry-mode unavailable --out /tmp/issue34/nightly.json
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
EXIT_CONFIG = 78
EXIT_INTERRUPTED = 130
EDGE_BASENAMES = {"Zelvik-Edge", "zelvik-edge"}
SCHEMA = "issue34-full-edge/v1"
DEFAULT_EDGE_ARG = "start-internal"
DEFAULT_TLS_HOST = "localhost"
TELEMETRY_MODES = ("unavailable", "unimplemented", "success")
DEFAULT_TELEMETRY_MODE = "unavailable"
FORBIDDEN_HOMES = {
    Path("/"),
    Path("/opt/zelvik/edge"),
    Path("/var/lib/zelvik/edge"),
    Path("/etc/zelvik"),
}


class FullEdgeError(RuntimeError):
    """The fixture cannot run or violated its contract."""


class PrerequisiteError(FullEdgeError):
    """A required binary, tool, or host capability is missing."""


def _print_json(document: dict) -> None:
    print(json.dumps(document, sort_keys=True))
    sys.stdout.flush()


def _atomic_write(path: Path, document: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def _atomic_write_text(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(text, encoding="utf-8")
    os.replace(temporary, path)


def _telemetry_mode(value: str) -> str:
    if value not in TELEMETRY_MODES:
        raise PrerequisiteError(
            f"unknown telemetry mode {value!r}; expected one of {', '.join(TELEMETRY_MODES)}"
        )
    return value


def _read_json(path: Path) -> dict | None:
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def _wait_json(path: Path, timeout: float, statuses=()) -> dict:
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        document = _read_json(path)
        if document is not None and (not statuses or document.get("status") in statuses):
            return document
        if document is None and path.exists():
            last_error = "file exists but is not valid JSON yet"
        time.sleep(0.1)
    raise FullEdgeError(
        f"timed out after {timeout:.0f}s waiting for {path}"
        + (f" (status in {sorted(statuses)})" if statuses else "")
        + (f": {last_error}" if last_error else "")
    )


def _default_controller_bin() -> Path | None:
    override = os.environ.get("ISSUE34_CONTROLLER_BIN")
    candidates = [Path(override)] if override else []
    candidates.extend(
        REPO_ROOT / profile / "examples" / "issue34-controller"
        for profile in ("debug", "release")
    )
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate
    return None


def _resolve_controller(value: str | None) -> Path:
    candidate = Path(value) if value else _default_controller_bin()
    if candidate is None:
        raise PrerequisiteError(
            "the fixture controller binary is missing; build it with "
            "`CARGO_BUILD_JOBS=2 cargo build --example issue34-controller` "
            "or pass --controller-bin"
        )
    candidate = candidate.expanduser().resolve()
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise PrerequisiteError(f"controller binary is not executable: {candidate}")
    return candidate


def _resolve_edge(value: str | None) -> Path:
    if not value:
        raise PrerequisiteError("--edge-binary is required (the real Zelvik-Edge binary)")
    candidate = Path(value).expanduser().resolve()
    if not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise PrerequisiteError(f"Edge binary is not executable: {candidate}")
    if candidate.name not in EDGE_BASENAMES:
        raise PrerequisiteError(
            "the full-edge lane requires the production binary named "
            f"Zelvik-Edge/zelvik-edge, got {candidate.name!r}; bench-defense is "
            "the component lane"
        )
    return candidate


def _check_home(home: Path) -> None:
    resolved = home.expanduser().resolve()
    if not resolved.is_absolute() or resolved == Path("/"):
        raise PrerequisiteError(f"home must be an absolute non-root path: {resolved}")
    if resolved in FORBIDDEN_HOMES and not (resolved / ".issue34-fixture").exists():
        raise PrerequisiteError(
            f"refusing to use the installed node home {resolved} as a fixture home"
        )
    if resolved.exists() and not resolved.is_dir():
        raise PrerequisiteError(f"home is not a directory: {resolved}")
    if resolved.is_dir() and any(resolved.iterdir()) and not (resolved / ".issue34-fixture").exists():
        raise PrerequisiteError(
            f"fixture home is not empty and is not marked as a fixture: {resolved}"
        )


def _make_home(value: str | None) -> tuple[Path, bool]:
    if value:
        home = Path(value).expanduser().resolve()
        _check_home(home)
        home.mkdir(parents=True, exist_ok=True)
        return home, False
    return Path(tempfile.mkdtemp(prefix="issue34-full-edge-")).resolve(), True


def _cleanup_home(home: Path, temporary: bool, keep: bool) -> None:
    if temporary and not keep:
        shutil.rmtree(home, ignore_errors=True)


def _pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _terminate_process(process: subprocess.Popen, timeout: float = 20.0) -> None:
    if process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=timeout)
        return
    except subprocess.TimeoutExpired:
        pass
    if process.poll() is None:
        process.kill()
        process.wait(timeout=10)


def _pid_cmdline(pid: int) -> str:
    proc = Path(f"/proc/{pid}/cmdline")
    if proc.is_file():
        try:
            return proc.read_bytes().replace(b"\0", b" ").decode("utf-8", "replace")
        except OSError:
            return ""
    try:
        result = subprocess.run(
            ["ps", "-o", "command=", "-p", str(pid)],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return result.stdout.strip() if result.returncode == 0 else ""


def _looks_like_controller(pid: int) -> bool:
    command = _pid_cmdline(pid)
    return "issue34-controller" in command


def _terminate_pid(pid: int, timeout: float = 20.0) -> bool:
    if not _pid_alive(pid):
        return False
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return False
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not _pid_alive(pid):
            return True
        time.sleep(0.1)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    return True


def _tool_path(name: str) -> str | None:
    return shutil.which(name)


def _required_status_fields() -> str:
    """Use status.py's default field list without duplicating it.

    ``soak.py`` currently calls ``status.parse_required_fields(None)`` when a
    status source is supplied without this flag; pass the defaults explicitly.
    """
    path = REPO_ROOT / "scripts" / "memory" / "status.py"
    spec = importlib.util.spec_from_file_location("issue34_status_defaults", path)
    if spec is None or spec.loader is None:
        raise PrerequisiteError(f"cannot load status defaults from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    fields = getattr(module, "DEFAULT_REQUIRED_FIELDS", None)
    if not fields:
        raise PrerequisiteError(f"{path} has no DEFAULT_REQUIRED_FIELDS")
    return ",".join(fields)


def _controller_command(
    controller: Path,
    *,
    home: Path,
    edge_arg: str,
    tls_host: str,
    tls_port: int = 0,
    edge_binary: Path | None = None,
    spawn_edge: bool = False,
    smoke: bool = False,
    ready_file: Path | None = None,
    status_file: Path | None = None,
    report_file: Path | None = None,
    grpc_listen: str | None = None,
    status_listen: str | None = None,
    telemetry_mode: str | None = None,
    telemetry_mode_file: Path | None = None,
    telemetry_store: Path | None = None,
) -> list[str]:
    command = [
        str(controller),
        "--home",
        str(home),
        "--edge-arg",
        edge_arg,
        "--tls-host",
        tls_host,
    ]
    if telemetry_mode is not None:
        command.extend(["--telemetry-mode", telemetry_mode])
    if telemetry_mode_file is not None:
        command.extend(["--telemetry-mode-file", str(telemetry_mode_file)])
    if telemetry_store is not None:
        command.extend(["--telemetry-store", str(telemetry_store)])
    if edge_binary is not None:
        command.extend(["--edge-binary", str(edge_binary)])
    if spawn_edge:
        command.append("--spawn-edge")
    if smoke:
        command.append("--smoke")
    if tls_port:
        command.extend(["--tls-port", str(tls_port)])
    if ready_file is not None:
        command.extend(["--ready-file", str(ready_file)])
    if status_file is not None:
        command.extend(["--status-file", str(status_file)])
    if report_file is not None:
        command.extend(["--report-file", str(report_file)])
    if grpc_listen:
        command.extend(["--grpc-listen", grpc_listen])
    if status_listen:
        command.extend(["--status-listen", status_listen])
    return command


def _run_with_group(
    command: list[str],
    *,
    timeout: float,
    stdout_path: Path,
    stderr_path: Path,
) -> tuple[int, str, str]:
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            command,
            stdout=stdout,
            stderr=stderr,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            return process.wait(timeout=timeout), "", ""
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)
            raise FullEdgeError(
                f"fixture exceeded {timeout:.0f}s; see {stdout_path} and {stderr_path}"
            )


def _check_summaries(report: dict) -> list[dict]:
    container = report.get("checks")
    items = container.get("checks") if isinstance(container, dict) else None
    if not isinstance(items, list):
        return []
    summaries = []
    for item in items:
        if not isinstance(item, dict):
            continue
        summaries.append(
            {
                key: item[key]
                for key in (
                    "name",
                    "ok",
                    "http_version",
                    "http_code",
                    "ssl_verify_result",
                    "payload_bytes",
                    "payload_sha256",
                    "alpn",
                    "peer_verified",
                )
                if key in item
            }
        )
    return summaries


def _report_summary(report: dict) -> dict:
    return {
        "status": report.get("status"),
        "scope": report.get("scope"),
        "edge_pid": report.get("edge_pid"),
        "pid_at_exit": report.get("pid_at_exit"),
        "node_health_observed": report.get("node_health_observed"),
        "config_applied": report.get("config_applied"),
        "duration_seconds": report.get("duration_seconds"),
        "home": report.get("home"),
        "tls": report.get("tls"),
        "origin": report.get("origin"),
        "snapshot_sha256": report.get("snapshot_sha256"),
        "counters": report.get("counters"),
        "checks": _check_summaries(report),
        "telemetry": report.get("telemetry"),
        "failures": report.get("failures"),
    }


def _validate_full_report(report: dict) -> list[str]:
    """Reject a report that did not observe the real production path.

    The ``full-edge`` target-kind label in soak.py is only a basename guard.
    This function is the actual full-startup assertion: signed config applied,
    node identity, both HTTP protocols on the real TLS port, terminal telemetry
    rejection, and one fixed PID for the whole run.
    """
    errors: list[str] = []

    def need(condition: bool, message: str) -> None:
        if not condition:
            errors.append(message)

    need(report.get("schema") == "issue34-full-edge-fixture/v1", "fixture schema mismatch")
    need(report.get("status") == "pass", "fixture status is not pass")
    scope = report.get("scope") or {}
    need(scope.get("full_edge") is True, "scope.full_edge is not true")
    need(scope.get("production_startup") is True, "scope.production_startup is not true")
    need(scope.get("component_lane") is False, "scope.component_lane is not false")
    need(report.get("node_health_observed") is True, "no NodeControl health report was observed")
    need(report.get("config_applied") is True, "the signed snapshot was not applied")
    edge_pid = report.get("edge_pid")
    need(isinstance(edge_pid, int) and edge_pid > 0, "edge_pid is missing")
    need(report.get("pid_at_exit") == edge_pid, "Edge PID changed during the run")
    counters = report.get("counters") or {}
    for counter in ("registerNode", "getDesiredRelease", "reportNodeHealth"):
        need(
            isinstance(counters.get(counter), int) and counters[counter] >= 1,
            f"NodeControl counter {counter} did not advance",
        )
    checks_container = report.get("checks") or {}
    checks = checks_container.get("checks") if isinstance(checks_container, dict) else None
    need(isinstance(checks, list) and bool(checks), "fixture checks are missing")
    by_name = {
        check.get("name"): check
        for check in (checks or [])
        if isinstance(check, dict) and check.get("name")
    }
    http1 = by_name.get("http1-post") or {}
    http2 = by_name.get("http2-post") or {}
    need(http1.get("ok") is True, "HTTP/1.1 request did not pass")
    need(
        isinstance(http1.get("http_version"), str)
        and http1["http_version"].startswith("1."),
        "HTTP/1.1 was not negotiated",
    )
    need(http1.get("http_code") == 200, "HTTP/1.1 did not return 200")
    need(http2.get("ok") is True, "HTTP/2 request did not pass")
    need(http2.get("http_version") == "2", "HTTP/2 was not negotiated")
    need(http2.get("http_code") == 200, "HTTP/2 did not return 200")
    need(
        http1.get("payload_sha256") == http2.get("payload_sha256")
        and bool(http1.get("payload_sha256")),
        "HTTP/1.1 and HTTP/2 payloads differ",
    )
    need((by_name.get("telemetry-reject") or {}).get("ok") is True, "telemetry rejection did not pass")
    retry = by_name.get("telemetry-retryable-recovery") or {}
    need(retry.get("ok") is True, "telemetry retry/recovery did not pass")
    need(
        isinstance(retry.get("pending_before"), int) and retry["pending_before"] >= 1,
        "telemetry pending counter did not grow",
    )
    need(
        isinstance(retry.get("max_batch_bytes"), int)
        and 0 < retry["max_batch_bytes"] <= 8 * 1024 * 1024,
        "telemetry batch size is not bounded",
    )
    need(retry.get("recovered") is True, "telemetry did not recover through the durable store")
    need(retry.get("pending_after") == 0, "telemetry pending did not drain after recovery")
    need(
        retry.get("core_pending_after") == 0
        and retry.get("detail_pending_after") == 0,
        "a telemetry lane still has pending batches after recovery",
    )
    need(
        isinstance(retry.get("acked_sequence"), int)
        and isinstance(retry.get("max_sequence_before"), int)
        and retry["acked_sequence"] >= retry["max_sequence_before"],
        "telemetry recovery did not acknowledge the retained backlog",
    )
    need(retry.get("bounded_status") is True, "telemetry status is not bounded scalars")
    need((by_name.get("node-status") or {}).get("ok") is True, "node status contract did not pass")
    telemetry = report.get("telemetry") or {}
    mode = telemetry.get("mode")
    expected = {"unavailable": (14, "UNAVAILABLE"), "unimplemented": (12, "UNIMPLEMENTED")}
    need(mode in expected, f"unexpected telemetry mode {mode!r}")
    if mode in expected:
        code, name = expected[mode]
        need(telemetry.get("served") is False, "telemetry is marked as served")
        need(
            telemetry.get("expected_code_number") == code,
            f"telemetry {mode} code is not {name}",
        )
    counters = telemetry.get("counters") or {}
    need(
        bool(counters) and all(isinstance(value, (int, float)) for value in counters.values()),
        "telemetry counters are not bounded scalars",
    )
    tls = report.get("tls") or {}
    for field in ("host", "port", "ca", "cert"):
        need(tls.get(field) not in (None, ""), f"TLS field {field} is missing")
    return errors


def preflight(args) -> int:
    checks = []

    def record(name: str, ok: bool, detail: str, required: bool = True) -> None:
        checks.append({"name": name, "ok": ok, "required": required, "detail": detail})

    controller = None
    try:
        controller = _resolve_controller(args.controller_bin)
        record("controller", True, str(controller))
    except PrerequisiteError as exc:
        record("controller", False, str(exc))

    edge = None
    if args.edge_binary is not None:
        try:
            edge = _resolve_edge(args.edge_binary)
            record("edge", True, str(edge))
        except PrerequisiteError as exc:
            record("edge", False, str(exc))
    else:
        record("edge", False, "--edge-binary not supplied", required=False)

    for tool in ("curl", "openssl"):
        path = _tool_path(tool)
        record(tool, path is not None, path or f"{tool} not found")
    curl = _tool_path("curl")
    if curl:
        try:
            version = subprocess.run(
                [curl, "--version"], capture_output=True, text=True, timeout=10
            )
            record(
                "curl_http2",
                version.returncode == 0 and "HTTP2" in version.stdout,
                version.stdout.splitlines()[0] if version.stdout else "no output",
            )
        except (OSError, subprocess.SubprocessError) as exc:
            record("curl_http2", False, str(exc))

    run_soak = REPO_ROOT / "scripts" / "memory" / "run_soak.py"
    record("run_soak", run_soak.is_file(), str(run_soak))

    linux = sys.platform.startswith("linux")
    record("linux_proc", (not linux) or Path("/proc/self/stat").is_file(), sys.platform, required=linux)

    if args.home is not None:
        try:
            _check_home(Path(args.home))
            record("home", True, str(Path(args.home).expanduser().resolve()))
        except PrerequisiteError as exc:
            record("home", False, str(exc))

    missing = [check["name"] for check in checks if check["required"] and not check["ok"]]
    document = {
        "schema": "issue34-full-edge-preflight/v1",
        "status": "pass" if not missing else "missing-prerequisites",
        "platform": sys.platform,
        "repo_root": str(REPO_ROOT),
        "controller_bin": str(controller) if controller else None,
        "edge_bin": str(edge) if edge else None,
        "checks": checks,
        "missing": missing,
        "note": (
            "The full-edge lane starts the production Zelvik-Edge binary from a "
            "throwaway home; bench-defense is the component lane and is rejected."
        ),
    }
    _print_json(document)
    return 0 if not missing else EXIT_CONFIG


def smoke(args) -> int:
    controller = _resolve_controller(args.controller_bin)
    edge = _resolve_edge(args.edge_binary)
    home, temporary = _make_home(args.home)
    report_path = (
        Path(args.report).expanduser().resolve()
        if args.report
        else Path(tempfile.gettempdir())
        / f"issue34-full-edge-smoke-{os.getpid()}-{int(time.time())}.json"
    )
    work = Path(tempfile.mkdtemp(prefix="issue34-smoke-work-")).resolve()
    started = time.monotonic()
    telemetry_store = home / "data" / "telemetry-store.bin"
    command = _controller_command(
        controller,
        home=home,
        edge_arg=args.edge_arg,
        tls_host=args.tls_host,
        tls_port=args.tls_port,
        edge_binary=edge,
        spawn_edge=True,
        smoke=True,
        report_file=report_path,
        telemetry_mode=_telemetry_mode(args.telemetry_mode),
        telemetry_store=telemetry_store,
    )
    error = None
    exit_code = None
    try:
        exit_code, _, _ = _run_with_group(
            command,
            timeout=args.timeout_seconds,
            stdout_path=work / "controller.stdout.log",
            stderr_path=work / "controller.stderr.log",
        )
    except FullEdgeError as exc:
        error = str(exc)
    duration = time.monotonic() - started
    report = _read_json(report_path)
    if report is None:
        report = {
            "schema": SCHEMA,
            "status": "fail",
            "failures": [error or "the fixture did not write a report"],
            "home": str(home),
            "duration_seconds": duration,
        }
    validation_errors = _validate_full_report(report)
    status = (
        report.get("status") == "pass"
        and exit_code == 0
        and error is None
        and not validation_errors
    )
    summary = {
        "schema": "issue34-full-edge-smoke/v1",
        "status": "pass" if status else "fail",
        "controller_exit_code": exit_code,
        "duration_seconds": duration,
        "error": error,
        "validation_errors": validation_errors,
        "home": str(home),
        "report_path": str(report_path),
        "controller_stdout": str(work / "controller.stdout.log"),
        "controller_stderr": str(work / "controller.stderr.log"),
        **_report_summary(report),
    }
    _print_json(summary)
    if status:
        _cleanup_home(home, temporary, args.keep_home)
    else:
        # Keep a failed home (and its Edge logs) for diagnosis unless the
        # operator explicitly asked for cleanup. It is a throwaway directory.
        if args.keep_home is False and args.clean_home_on_failure:
            _cleanup_home(home, temporary, False)
    return 0 if status else 1


def _start_controller_serve(
    *,
    controller: Path,
    edge: Path,
    home: Path,
    edge_arg: str,
    tls_host: str,
    ready_file: Path,
    status_file: Path,
    report_file: Path,
    log_file: Path,
    ready_timeout: float,
    grpc_listen: str | None,
    telemetry_mode: str,
    telemetry_mode_file: Path,
    telemetry_store: Path,
) -> tuple[subprocess.Popen, dict]:
    command = _controller_command(
        controller,
        home=home,
        edge_arg=edge_arg,
        tls_host=tls_host,
        ready_file=ready_file,
        status_file=status_file,
        report_file=report_file,
        grpc_listen=grpc_listen,
        status_listen=None,
        telemetry_mode=telemetry_mode,
        telemetry_mode_file=telemetry_mode_file,
        telemetry_store=telemetry_store,
    )
    log_file.parent.mkdir(parents=True, exist_ok=True)
    handle = log_file.open("ab")
    process = subprocess.Popen(
        command,
        stdout=handle,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    handle.close()
    try:
        ready = _wait_json(
            ready_file,
            ready_timeout,
            statuses=("controller-ready", "edge-ready"),
        )
    except FullEdgeError:
        _terminate_process(process)
        raise
    contract = {
        "schema": "issue34-full-edge-contract/v1",
        "status": ready.get("status"),
        "controller_pid": process.pid,
        "controller_bin": str(controller),
        "edge_bin": str(edge),
        "edge_arg": edge_arg,
        "home": str(home),
        "ready_file": str(ready_file),
        "status_file": str(status_file),
        "controller_report": str(report_file),
        "controller_log": str(log_file),
        "grpc_addr": ready.get("grpc_addr"),
        "status_url": ready.get("status_url"),
        "tls": ready.get("tls"),
        "snapshot_sha256": ready.get("snapshot_sha256"),
        "run_soak": {
            "target_cmd": str(edge),
            "target_kind": "full-edge",
            "target_args": [edge_arg],
            "target_env": [f"ZELVIK_EDGE_HOME={home}"],
            "target_pid_file": str(home / "data" / "zelvik-edge.pid"),
            "tls_host": tls_host,
            "tls_port": (ready.get("tls") or {}).get("port"),
            "tls_ca": (ready.get("tls") or {}).get("ca"),
            "status_source": ready.get("status_url"),
            "telemetry_status": ready.get("status_url"),
            "telemetry_mode": telemetry_mode,
            "telemetry_mode_file": str(telemetry_mode_file),
            "telemetry_store": str(telemetry_store),
            "origin": "external",
            "require_node_status": False,
            "note": (
                "pass --require-node-status only when workerReclaim.coversAllWorkers "
                "is true for this target; the real Edge currently reports the "
                "unhooked Pingora workers honestly, so the strict hook-coverage "
                "check would fail on a correct snapshot."
            ),
        },
        "telemetry": ready.get("telemetry"),
        "telemetry_mode": telemetry_mode,
        "telemetry_mode_file": str(telemetry_mode_file),
        "telemetry_store": str(telemetry_store),
    }
    return process, contract


def _serve_final(process: subprocess.Popen, contract: dict, status_file: Path) -> dict:
    _terminate_process(process)
    status_document = _read_json(status_file) or {}
    controller_report = _read_json(Path(contract["controller_report"])) or {}
    edge_ready = bool(status_document.get("ready")) or (
        controller_report.get("node_health_observed") is True
    )
    return {
        "schema": "issue34-full-edge-serve/v1",
        "status": "pass" if edge_ready else "fail",
        "controller_pid": process.pid,
        "controller_exit_code": process.returncode,
        "edge_ready_observed": edge_ready,
        "edge_pid": controller_report.get("edge_pid")
        or status_document.get("edge_pid"),
        "pid_at_exit": controller_report.get("pid_at_exit"),
        "node_status_ready": status_document.get("ready"),
        "counters": controller_report.get("counters"),
        "controller_report": contract["controller_report"],
        "status_file": str(status_file),
        "home": contract["home"],
        "failures": controller_report.get("failures"),
    }


def serve(args) -> int:
    controller = _resolve_controller(args.controller_bin)
    edge = _resolve_edge(args.edge_binary)
    home, temporary = _make_home(args.home)
    run_dir = Path(args.run_dir).expanduser().resolve() if args.run_dir else home
    ready_file = (
        Path(args.ready_file).expanduser().resolve()
        if args.ready_file
        else run_dir / "controller-ready.json"
    )
    status_file = (
        Path(args.status_file).expanduser().resolve()
        if args.status_file
        else run_dir / "node-status.json"
    )
    report_file = (
        Path(args.report_file).expanduser().resolve()
        if args.report_file
        else run_dir / "controller-report.json"
    )
    contract_file = (
        Path(args.contract_file).expanduser().resolve()
        if args.contract_file
        else run_dir / "contract.json"
    )
    telemetry_mode = _telemetry_mode(args.telemetry_mode)
    telemetry_mode_file = run_dir / "telemetry-mode"
    telemetry_store = run_dir / "telemetry-store.bin"
    _atomic_write_text(telemetry_mode_file, telemetry_mode + "\n")
    process = None
    contract = None
    stop = {"value": False}
    exit_code = 1

    def _request_stop(signum, _frame) -> None:
        stop["value"] = True

    previous = {}
    for name in (signal.SIGTERM, signal.SIGINT):
        previous[name] = signal.signal(name, _request_stop)
    try:
        process, contract = _start_controller_serve(
            controller=controller,
            edge=edge,
            home=home,
            edge_arg=args.edge_arg,
            tls_host=args.tls_host,
            ready_file=ready_file,
            status_file=status_file,
            report_file=report_file,
            log_file=run_dir / "controller.log",
            ready_timeout=args.ready_timeout_seconds,
            grpc_listen=args.grpc_listen,
            telemetry_mode=telemetry_mode,
            telemetry_mode_file=telemetry_mode_file,
            telemetry_store=telemetry_store,
        )
        _atomic_write(contract_file, contract)
        _print_json(contract)
        if args.serve_seconds > 0:
            deadline = time.monotonic() + args.serve_seconds
            while (
                not stop["value"]
                and process.poll() is None
                and time.monotonic() < deadline
            ):
                time.sleep(0.2)
        else:
            while not stop["value"] and process.poll() is None:
                time.sleep(0.2)
        exit_code = 0
    finally:
        for name, handler in previous.items():
            signal.signal(name, handler)
        if process is not None:
            summary = _serve_final(process, contract or {}, status_file)
            summary["controller_exit_code"] = process.returncode
            _print_json(summary)
            _cleanup_home(home, temporary, args.keep_home)
            exit_code = 0 if summary["status"] == "pass" else 1
    return exit_code


def _http_json(url: str | None, timeout: float = 2.0) -> dict | None:
    if not url:
        return None
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response:
            value = json.loads(response.read().decode("utf-8"))
    except (OSError, ValueError):
        return None
    return value if isinstance(value, dict) else None


def _telemetry_counters(status_url: str | None) -> dict:
    document = _http_json(status_url) or {}
    telemetry = document.get("telemetry") or {}
    counters = telemetry.get("counters") or {}
    return counters if isinstance(counters, dict) else {}


def _wait_telemetry_recovery(
    status_url: str | None,
    *,
    timeout: float,
    pending_before: int,
) -> dict:
    started = time.monotonic()
    deadline = started + timeout
    last: dict = {}
    while time.monotonic() < deadline:
        counters = _telemetry_counters(status_url)
        last = counters
        max_sequence = counters.get("maxSequence")
        acked = counters.get("ackedSequence")
        if (
            pending_before > 0
            and counters.get("pendingBatches") == 0
            and isinstance(max_sequence, int)
            and isinstance(acked, int)
            and acked >= max_sequence
            and counters.get("success", 0) >= 1
        ):
            return {
                "status": "pass",
                "mode": "unavailable->success",
                "pending_before": pending_before,
                "pending_after": 0,
                "acked_sequence": acked,
                "max_sequence": max_sequence,
                "success": counters.get("success"),
                "max_batch_bytes": counters.get("maxBatchBytes"),
                "bounded_status": all(
                    isinstance(value, (int, float)) for value in counters.values()
                ),
                "duration_seconds": time.monotonic() - started,
            }
        time.sleep(0.25)
    return {
        "status": "fail",
        "mode": "unavailable->success",
        "pending_before": pending_before,
        "error": "telemetry did not acknowledge the retained backlog before the soak ended",
        "last_counters": last,
        "duration_seconds": time.monotonic() - started,
    }


def _run_soak_with_recovery(
    command: list[str],
    *,
    duration: float,
    recovery_seconds: float,
    mode_file: Path,
    status_url: str | None,
) -> tuple[int, dict | None]:
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL)
    recovery = None
    if recovery_seconds > 0:
        lead = min(10.0, max(3.0, duration * 0.2))
        switch_at = time.monotonic() + max(0.0, duration - recovery_seconds - lead)
        while time.monotonic() < switch_at and process.poll() is None:
            time.sleep(0.5)
        if process.poll() is not None:
            recovery = {
                "status": "fail",
                "error": f"soak exited before the recovery switch (status {process.returncode})",
            }
        else:
            pending_before = int(_telemetry_counters(status_url).get("pendingBatches") or 0)
            _atomic_write_text(mode_file, "success\n")
            recovery = _wait_telemetry_recovery(
                status_url,
                timeout=max(5.0, recovery_seconds + lead - 2.0),
                pending_before=pending_before,
            )
    return process.wait(), recovery


def soak(args) -> int:
    controller = _resolve_controller(args.controller_bin)
    edge = _resolve_edge(args.edge_binary)
    home, temporary = _make_home(args.home)
    run_dir = Path(args.run_dir).expanduser().resolve() if args.run_dir else home
    run_dir.mkdir(parents=True, exist_ok=True)
    ready_file = run_dir / "controller-ready.json"
    status_file = run_dir / "node-status.json"
    report_file = run_dir / "controller-report.json"
    contract_file = run_dir / "contract.json"
    telemetry_mode = _telemetry_mode(args.telemetry_mode)
    if args.recovery_seconds < 0 or args.recovery_seconds >= args.duration:
        raise PrerequisiteError("--recovery-seconds must be >= 0 and shorter than --duration")
    telemetry_mode_file = run_dir / "telemetry-mode"
    telemetry_store = run_dir / "telemetry-store.bin"
    _atomic_write_text(telemetry_mode_file, telemetry_mode + "\n")
    process = None
    try:
        process, contract = _start_controller_serve(
            controller=controller,
            edge=edge,
            home=home,
            edge_arg=args.edge_arg,
            tls_host=args.tls_host,
            ready_file=ready_file,
            status_file=status_file,
            report_file=report_file,
            log_file=run_dir / "controller.log",
            ready_timeout=args.ready_timeout_seconds,
            grpc_listen=None,
            telemetry_mode=telemetry_mode,
            telemetry_mode_file=telemetry_mode_file,
            telemetry_store=telemetry_store,
        )
        _atomic_write(contract_file, contract)
        target = contract["run_soak"]
        if not target["tls_port"] or not target["tls_ca"] or not target["status_source"]:
            raise FullEdgeError("controller contract is missing TLS or status endpoints")
        command = [
            sys.executable,
            "-I",
            str(REPO_ROOT / "scripts" / "memory" / "run_soak.py"),
            "--target-cmd",
            target["target_cmd"],
            "--target-arg",
            args.edge_arg,
            "--target-kind",
            "full-edge",
            "--target-env",
            f"ZELVIK_EDGE_HOME={home}",
            "--target-pid-file",
            target["target_pid_file"],
            "--target-log",
            str(run_dir / "edge.log"),
            "--origin",
            "external",
            "--tls-host",
            args.tls_host,
            "--tls-port",
            str(target["tls_port"]),
            "--tls-ca",
            target["tls_ca"],
            "--status-source",
            target["status_source"],
            "--require-status-fields",
            _required_status_fields(),
            "--status-interval",
            str(args.status_interval),
            "--samples-jsonl",
            str(run_dir / "samples.jsonl"),
            "--progress-file",
            str(run_dir / "progress.json"),
            "--duration",
            str(args.duration),
            "--out",
            str(Path(args.out).expanduser().resolve()),
        ]
        if args.warmup_seconds is not None:
            command.extend(["--warmup-seconds", str(args.warmup_seconds)])
        if args.min_span_seconds is not None:
            command.extend(["--min-span-seconds", str(args.min_span_seconds)])
        if args.budget_mib_per_hour is not None:
            command.extend(["--budget-mib-per-hour", str(args.budget_mib_per_hour)])
        if args.max_rss_mib is not None:
            command.extend(["--max-rss-mib", str(args.max_rss_mib)])
        if args.pressure_cycles is not None:
            command.extend(["--pressure-cycles", str(args.pressure_cycles)])
        if args.require_node_status:
            command.append("--require-node-status")
        for extra in args.soak_arg or []:
            command.append(extra)

        started = time.monotonic()
        soak_exit_code, recovery = _run_soak_with_recovery(
            command,
            duration=args.duration,
            recovery_seconds=args.recovery_seconds,
            mode_file=telemetry_mode_file,
            status_url=contract.get("status_url"),
        )
        duration = time.monotonic() - started
        soak_report = _read_json(Path(args.out).expanduser().resolve()) or {}
        summary = {
            "schema": "issue34-full-edge-soak/v1",
            "status": soak_report.get("verdict") or "fail",
            "soak_exit_code": soak_exit_code,
            "duration_seconds": duration,
            "contract": contract,
            "contract_file": str(contract_file),
            "soak_report": str(Path(args.out).expanduser().resolve()),
            "soak_scope": soak_report.get("scope"),
            "same_pid": (soak_report.get("target") or {}).get("same_pid"),
            "edge_pid": (soak_report.get("target") or {}).get("pid"),
            "violations": soak_report.get("violations"),
            "node_status": soak_report.get("node_status"),
            "require_node_status": args.require_node_status,
            "telemetry_mode": telemetry_mode,
            "telemetry_mode_file": str(telemetry_mode_file),
            "telemetry_store": str(telemetry_store),
            "recovery_seconds": args.recovery_seconds,
            "telemetry_recovery": recovery,
        }
        final = _serve_final(process, contract, status_file)
        process = None
        summary["controller"] = final
        if final["status"] != "pass":
            summary["status"] = "fail"
        if recovery is not None and recovery.get("status") != "pass":
            summary["status"] = "fail"
        _print_json(summary)
        if summary["status"] == "pass" and soak_exit_code == 0:
            return 0
        return 1
    except KeyboardInterrupt:
        if process is not None:
            _terminate_process(process)
        return EXIT_INTERRUPTED
    finally:
        if process is not None:
            _terminate_process(process)
        _cleanup_home(home, temporary, args.keep_home or args.keep_run_dir)


def stop(args) -> int:
    contract = _read_json(Path(args.contract_file).expanduser().resolve())
    if not contract:
        raise PrerequisiteError(f"contract file is missing or invalid: {args.contract_file}")
    if contract.get("schema") != "issue34-full-edge-contract/v1":
        raise PrerequisiteError("contract file is not an Issue 34 fixture contract")
    home = Path(str(contract.get("home") or ""))
    if not (home / ".issue34-fixture").is_file():
        raise PrerequisiteError(
            f"refusing to stop a process whose home is not a fixture: {home}"
        )
    pid = int(contract.get("controller_pid") or 0)
    if pid and not _looks_like_controller(pid):
        raise PrerequisiteError(
            f"refusing to stop pid {pid}: its command line is not the fixture controller"
        )
    terminated = _terminate_pid(pid, args.timeout_seconds) if pid else False
    result = {
        "schema": "issue34-full-edge-stop/v1",
        "status": "pass",
        "controller_pid": pid,
        "terminated": terminated,
        "still_running": _pid_alive(pid),
    }
    _print_json(result)
    return 0 if not result["still_running"] else 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    subparsers = parser.add_subparsers(dest="command", required=True)

    def add_common(subparser: argparse.ArgumentParser) -> None:
        subparser.add_argument("--edge-binary", required=False)
        subparser.add_argument("--controller-bin", default=None)
        subparser.add_argument("--home", default=None)
        subparser.add_argument("--edge-arg", default=DEFAULT_EDGE_ARG)
        subparser.add_argument("--tls-host", default=DEFAULT_TLS_HOST)
        subparser.add_argument(
            "--telemetry-mode",
            default=DEFAULT_TELEMETRY_MODE,
            choices=TELEMETRY_MODES,
            help=(
                "initial fixture backend; smoke exercises unavailable->success->"
                "unimplemented internally, soak keeps unavailable unless a "
                "recovery phase switches it"
            ),
        )
        subparser.add_argument("--keep-home", action="store_true")

    preflight_parser = subparsers.add_parser("preflight", help="check binaries and host tools")
    add_common(preflight_parser)
    preflight_parser.set_defaults(handler=preflight)

    smoke_parser = subparsers.add_parser(
        "smoke", help="spawn the real Edge and require HTTP/1.1, HTTP/2, TLS, health"
    )
    add_common(smoke_parser)
    smoke_parser.add_argument("--report", default=None)
    smoke_parser.add_argument("--tls-port", type=int, default=0)
    smoke_parser.add_argument("--timeout-seconds", type=float, default=300.0)
    smoke_parser.add_argument("--clean-home-on-failure", action="store_true")
    smoke_parser.set_defaults(handler=smoke)

    serve_parser = subparsers.add_parser(
        "serve", help="run only the fake control plane for an external run_soak.py"
    )
    add_common(serve_parser)
    serve_parser.add_argument("--run-dir", default=None)
    serve_parser.add_argument("--ready-file", default=None)
    serve_parser.add_argument("--status-file", default=None)
    serve_parser.add_argument("--report-file", default=None)
    serve_parser.add_argument("--contract-file", default=None)
    serve_parser.add_argument("--grpc-listen", default=None)
    serve_parser.add_argument("--ready-timeout-seconds", type=float, default=60.0)
    serve_parser.add_argument("--serve-seconds", type=float, default=0.0)
    serve_parser.set_defaults(handler=serve)

    soak_parser = subparsers.add_parser(
        "soak", help="serve the fixture, run the real Edge under run_soak.py, clean up"
    )
    add_common(soak_parser)
    soak_parser.add_argument("--run-dir", default=None)
    soak_parser.add_argument("--duration", type=float, required=True)
    soak_parser.add_argument(
        "--recovery-seconds",
        type=float,
        default=0.0,
        help=(
            "switch telemetry to the durable success backend this long before "
            "the soak ends and require the retained backlog to be acknowledged"
        ),
    )
    soak_parser.add_argument("--out", required=True)
    soak_parser.add_argument("--warmup-seconds", type=float, default=None)
    soak_parser.add_argument("--min-span-seconds", type=float, default=None)
    soak_parser.add_argument("--budget-mib-per-hour", type=float, default=None)
    soak_parser.add_argument("--max-rss-mib", type=float, default=None)
    soak_parser.add_argument("--pressure-cycles", type=int, default=None)
    soak_parser.add_argument("--status-interval", type=float, default=5.0)
    soak_parser.add_argument("--require-node-status", action="store_true")
    soak_parser.add_argument("--ready-timeout-seconds", type=float, default=60.0)
    soak_parser.add_argument("--keep-run-dir", action="store_true")
    soak_parser.add_argument(
        "--soak-arg",
        action="append",
        default=[],
        help="extra argument forwarded to run_soak.py (repeatable)",
    )
    soak_parser.set_defaults(handler=soak)

    stop_parser = subparsers.add_parser("stop", help="stop the controller from a contract file")
    stop_parser.add_argument("--contract-file", required=True)
    stop_parser.add_argument("--timeout-seconds", type=float, default=20.0)
    stop_parser.set_defaults(handler=stop)
    return parser


def _rewrite_controller_ready(argv: list[str]) -> list[str]:
    """Make ``--controller-ready`` mean an actual startup, never a label.

    The old stub returned success after binding a socket. Here the flag is
    rewritten to the ``smoke`` subcommand, which spawns the real binary and
    requires the real listener and health report. Using it with ``serve`` or
    ``stop`` is rejected because those do not themselves prove startup.
    """
    if "--controller-ready" not in argv:
        return argv
    remaining = [item for item in argv if item != "--controller-ready"]
    if remaining and remaining[0] in ("serve", "stop", "soak", "preflight"):
        raise FullEdgeError(
            "--controller-ready proves startup only through the smoke subcommand; "
            "do not combine it with serve/stop/soak/preflight"
        )
    if not remaining or remaining[0].startswith("-"):
        return ["smoke", *remaining]
    if remaining[0] == "smoke":
        return remaining
    return ["smoke", *remaining]


def main(argv=None) -> int:
    arguments = list(sys.argv[1:] if argv is None else argv)
    if arguments and arguments[0] in ("-h", "--help"):
        build_parser().print_help()
        return 0
    try:
        arguments = _rewrite_controller_ready(arguments)
        args = build_parser().parse_args(arguments)
        return args.handler(args)
    except PrerequisiteError as exc:
        _print_json(
            {
                "schema": "issue34-full-edge/v1",
                "status": "missing-prerequisites",
                "error": str(exc),
                "exit_code": EXIT_CONFIG,
            }
        )
        return EXIT_CONFIG
    except FullEdgeError as exc:
        _print_json(
            {
                "schema": "issue34-full-edge/v1",
                "status": "fail",
                "error": str(exc),
                "exit_code": 1,
            }
        )
        return 1
    except KeyboardInterrupt:
        return EXIT_INTERRUPTED


if __name__ == "__main__":
    raise SystemExit(main())
