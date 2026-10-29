#!/usr/bin/env python3
"""Linux-only runner. Dedicated disposable test server; all sockets are loopback.
Usage: run_memory_pressure.py --candidate BIN [--baseline BIN] --output DIR
Requires systemd-run, perf and memory_pressure.py's isolated Python environment.
"""
import argparse
import json
import pathlib
import socket
import subprocess
import sys
import time


def run(args):
    out = pathlib.Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    driver = pathlib.Path(args.driver).resolve()
    variants = [("candidate", pathlib.Path(args.candidate).resolve())]
    if args.baseline:
        variants.insert(0, ("baseline", pathlib.Path(args.baseline).resolve()))
    for label, binary in variants:
        if not binary.is_file():
            raise FileNotFoundError(binary)
    tls = out / "tls"
    tls.mkdir(exist_ok=True)
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-keyout", str(tls / "key.pem"), "-out", str(tls / "cert.pem"), "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"], check=True, capture_output=True)
    (tls / "key.pem").chmod(0o600)
    metadata = {"variants": {}, "cpu": pathlib.Path("/proc/cpuinfo").read_text(), "kernel": subprocess.check_output(["uname", "-a"], text=True), "rounds": args.rounds, "limits": [256, 512], "note": "opt-level=2, debug=0, incremental=0; timing with allocation counting off, separate allocation runs"}
    for label, binary in variants:
        metadata["variants"][label] = {"binary": str(binary), "sha256": subprocess.check_output(["sha256sum", str(binary)], text=True).split()[0]}
    (out / "metadata.json").write_text(json.dumps(metadata, indent=2))
    tasks = []
    if args.mode in ["all", "pressure", "continuation"]:
        for limit in [512, 256]:
            for repeat in range(args.rounds):
                extra = ["--continuation-only"] if args.mode == "continuation" else []
                tasks.append(("candidate", pathlib.Path(args.candidate).resolve(), f"{args.mode}-{limit}-{repeat}", limit, extra, None))
    if args.mode in ["all", "abuse"]:
        tasks.append(("candidate", pathlib.Path(args.candidate).resolve(), "abuse", 512, ["--abuse"], None))
    if args.mode == "acceptance":
        # Distinguish the pressure source: production reclaimable L1 cache,
        # touched anonymous ballast, and both together. Only cache-bearing
        # sources enable the fixture's production cache configuration.
        limits = [args.limit] if args.limit else [512, 256]
        sources = [args.source] if args.source else ["cache", "ballast", "mixed"]
        for limit in limits:
            for source in sources:
                for repeat in range(args.rounds):
                    env = {"PRESSURE_ACCEPTANCE": "1"}
                    if source in ("cache", "mixed"):
                        env["PRESSURE_CACHE"] = "1"
                    tasks.append(("candidate", pathlib.Path(args.candidate).resolve(), f"acceptance-{source}-{limit}-{repeat}", limit, ["--acceptance-source", source], env))
    if args.baseline and args.mode in ["all", "benchmark"]:
        # Alternate AB / BA order; start a fresh process and cgroup each round.
        for repeat in range(args.rounds):
            for label, binary in (variants if repeat % 2 == 0 else list(reversed(variants))):
                tasks.append((label, binary, f"bench-{repeat}-{label}", 1024, ["--benchmark", "--requests", str(args.requests)], None))
        for repeat in range(3):
            for label, binary in (variants if repeat % 2 == 0 else list(reversed(variants))):
                tasks.append((label, binary, f"alloc-{repeat}-{label}", 1024, ["--benchmark", "--count", "--requests", "1000"], None))
    for label, binary, name, limit, extra, env in tasks:
        # The governor currently reads host PSI. Let its avg10 recover between
        # isolated cgroups, otherwise the previous run contaminates the next.
        recovery_start = time.monotonic()
        while True:
            psi = pathlib.Path("/proc/pressure/memory").read_text()
            averages = [float(part.split("=", 1)[1]) for line in psi.splitlines()
                        for part in line.split() if part.startswith("avg10=")]
            if max(averages, default=0) < 1:
                break
            if time.monotonic() - recovery_start > 180:
                raise RuntimeError(f"host PSI did not recover before {name}: {psi}")
            time.sleep(2)
        (out / (name + ".start.json")).write_text(json.dumps({"host_psi": psi, "recovery_seconds": time.monotonic() - recovery_start}))
        unit = "zelvik-memory-" + name
        node_home = out / (name + "-home")
        node_home.mkdir(exist_ok=True)
        log = out / (name + ".server.log")
        command = ["systemd-run", "--quiet", "--collect", "--unit", unit,
                   "-p", f"MemoryMax={limit}M", "-p", f"MemoryHigh={int(limit*.90)}M",
                   "-p", "MemorySwapMax=0", "-p", "CPUQuota=200%", "-p", "AllowedCPUs=0,1",
                   "-p", "TasksMax=128", "-p", "LimitNOFILE=65536", "-p", f"StandardOutput=append:{log}", "-p", f"StandardError=append:{log}",
                   "--setenv", f"ZELVIK_EDGE_HOME={node_home}", "--setenv", f"PRESSURE_TLS_DIR={tls}"]
        for key, value in (env or {}).items():
            command += ["--setenv", f"{key}={value}"]
        command.append(str(binary))
        subprocess.run(command, check=True)
        profiler = None
        state = None
        try:
            deadline = time.monotonic() + 25
            while True:
                try:
                    with socket.create_connection(("127.0.0.1", 19090), 1) as s:
                        s.sendall(b"stats\n")
                        state = json.loads(s.recv(65536))
                    break
                except (OSError, ValueError):
                    status = subprocess.check_output(["systemctl", "show", unit, "-p", "ActiveState", "--value"], text=True).strip()
                    if status == "failed" or time.monotonic() > deadline:
                        raise RuntimeError(f"{name}: fixture not ready: {log.read_text()[-4000:] if log.exists() else status}")
                    time.sleep(.1)
            # Software counters work on this VM; PMU cycles/cache misses do not.
            profiler = subprocess.Popen(["perf", "stat", "-x", ",", "-e", "task-clock,context-switches,cpu-migrations,page-faults,syscalls:sys_enter_futex", "-p", str(state["pid"]), "-o", str(out / (name + ".perf.csv"))], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            with (out / (name + ".client.log")).open("w") as client_log:
                result = subprocess.run([sys.executable, "-I", str(driver), "--label", label, "--output", str(out / (name + ".json")), *extra], stdout=client_log, stderr=subprocess.STDOUT, timeout=600)
            print(json.dumps({"run": name, "returncode": result.returncode}), flush=True)
            if result.returncode:
                raise RuntimeError(f"client failed: {name}; see {out / (name + '.client.log')}")
        finally:
            # Preserve failure/timeout counters too, before removing the cgroup.
            if state:
                cg = pathlib.Path(state["cgroup"])
                terminal = {p: (cg / p).read_text() if (cg / p).exists() else "cgroup gone" for p in ["memory.current", "memory.peak", "memory.events", "memory.pressure", "cpu.stat"]}
                terminal["unit"] = subprocess.run(["systemctl", "show", unit, "-p", "Result", "-p", "ExecMainStatus", "-p", "OOMKilled"], capture_output=True, text=True).stdout
                (out / (name + ".terminal.json")).write_text(json.dumps(terminal, indent=2))
            if profiler:
                profiler.send_signal(2)
                try:
                    profiler.communicate(timeout=5)
                except subprocess.TimeoutExpired:
                    profiler.kill()
                    profiler.communicate()
            subprocess.run(["systemctl", "stop", unit], check=False)
            subprocess.run(["systemctl", "reset-failed", unit], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--candidate", required=True)
    p.add_argument("--baseline")
    p.add_argument("--driver", required=True)
    p.add_argument("--output", required=True)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--requests", type=int, default=4000)
    p.add_argument("--mode", choices=["all", "pressure", "continuation", "abuse", "benchmark", "acceptance"], default="all")
    p.add_argument("--source", choices=["cache", "ballast", "mixed"], help="acceptance source filter")
    p.add_argument("--limit", type=int, help="acceptance cgroup limit in MiB")
    run(p.parse_args())
