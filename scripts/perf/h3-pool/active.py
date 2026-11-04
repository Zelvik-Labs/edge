#!/usr/bin/env python3
"""Same-binary exclusive versus active H3 origin reuse, actual wire exchanges."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import time

root = Path.cwd()
out = root / ".local/h3-active-ci"
out.mkdir(parents=True, exist_ok=False)
with (out / "build.jsonl").open("w") as stdout, (out / "build.log").open("w") as stderr:
    subprocess.run(["cargo", "test", "--locked", "--release", "--lib", "--no-run", "--message-format=json"],
                   stdout=stdout, stderr=stderr, check=True)
artifacts = [json.loads(line) for line in (out / "build.jsonl").read_text().splitlines()]
executables = [p["executable"] for p in artifacts if p.get("reason") == "compiler-artifact"
               and p.get("executable") and p["target"]["name"] == "zelvik_edge" and p["profile"]["test"]]
assert len(executables) == 1
binary = out / "active-tests"
shutil.copy2(executables[0], binary)
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
with (out / "regressions.log").open("w") as log:
    subprocess.run([str(binary), "origin_h3", "--test-threads=2", "--nocapture"],
                   env=dict(os.environ, EDGE_H3_TEST_OUTPUT=str(out / "wire")),
                   stdout=log, stderr=subprocess.STDOUT, check=True, timeout=90)
allowed = sorted(os.sched_getaffinity(0))
cpu = allowed[0]
metadata = {"commit": os.environ["GITHUB_SHA"], "binary_sha256": digest, "allowed_cpus": allowed,
            "pinned_cpu": cpu, "kernel": platform.uname()._asdict(), "rustflags": os.environ["RUSTFLAGS"],
            "profile": "release library test; System allocator; unwind", "rounds": 5,
            "scope": "same binary, active registry get enabled/disabled; actual H3, not whole Edge"}
(out / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
rows = []
for repeat in range(5):
    for mode in (["off", "on"] if repeat % 2 == 0 else ["on", "off"]):
        name = f"round-{repeat}-{mode}"
        result_path = out / f"{name}.json"
        before = {"time_ns": time.monotonic_ns(), "proc_stat": Path("/proc/stat").read_text()}
        with (out / f"{name}.log").open("w") as log:
            process = subprocess.run(["taskset", "-c", str(cpu), str(binary),
                "origin_h3::wire_tests::active_origin_reuse_cost_probe", "--exact", "--ignored", "--test-threads=1"],
                env=dict(os.environ, EDGE_H3_ACTIVE_REUSE=mode, EDGE_H3_ACTIVE_RESULT=str(result_path)),
                stdout=log, stderr=subprocess.STDOUT, timeout=210)
        after = {"time_ns": time.monotonic_ns(), "proc_stat": Path("/proc/stat").read_text()}
        (out / f"{name}-host.json").write_text(json.dumps({"before": before, "after": after,
            "exit_code": process.returncode}, indent=2) + "\n")
        process.check_returncode()
        result = json.loads(result_path.read_text())
        assert result["passed"] and result["active_reuse"] == (mode == "on")
        assert result["measured_requests"] == 3200 and result["warmup_requests"] == 1
        assert len(result["wave_ns"]) == 100
        assert result["accepted_and_closed_connections"] == (1 if mode == "on" else 3101)
        rows.append({"mode": mode, "round": repeat, "ns_per_request": sum(result["wave_ns"]) / 3200})
        (out / "records.json").write_text(json.dumps(rows, indent=2) + "\n")
ratios = []
for repeat in range(5):
    pair = {r["mode"]: r["ns_per_request"] for r in rows if r["round"] == repeat}
    ratios.append(pair["on"] / pair["off"])
assert hashlib.sha256(binary.read_bytes()).hexdigest() == digest
(out / "summary.json").write_text(json.dumps({"paired_ratios": ratios,
    "median": statistics.median(ratios), "processes": len(rows)}, indent=2) + "\n")
