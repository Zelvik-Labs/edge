#!/usr/bin/env python3
"""Balanced fresh-process real idle-pool comparison, with all raw samples."""
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import time

root = Path.cwd()
out = root / ".local/h3-pool-ci"
allowed = sorted(os.sched_getaffinity(0))
cpu = allowed[0]
soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
assert hard == resource.RLIM_INFINITY or hard >= 8192, (soft, hard)
resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, 8192), hard))
metadata = {"kernel": platform.uname()._asdict(), "allowed_cpus": allowed, "pinned_cpu": cpu,
            "fd_limit_before": [soft, hard], "fd_limit_after": resource.getrlimit(resource.RLIMIT_NOFILE),
            "source_commit": os.environ["GITHUB_SHA"], "rounds": 5,
            "scope": "take+offer ns per cycle; not HTTP QPS; setup/cleanup excluded",
            "binaries": {v: hashlib.sha256((out / f"{v}-tests").read_bytes()).hexdigest()
                         for v in ["baseline", "candidate"]}}
(out / "run-metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")

def counters():
    result = {"monotonic_ns": time.monotonic_ns(), "proc_stat": Path("/proc/stat").read_text()}
    cgroup = next(line.split(":", 2)[2] for line in Path("/proc/self/cgroup").read_text().splitlines()
                  if line.startswith("0::"))
    path = Path("/sys/fs/cgroup") / cgroup.lstrip("/") / "cpu.stat"
    result["cgroup_cpu_stat_path"] = str(path)
    result["cgroup_cpu_stat"] = path.read_text() if path.exists() else None
    return result

rows = []
for keys, cycles in [(1, 50_000), (16, 10_000), (256, 1024), (1024, 1024)]:
    for round_id in range(5):
        order = ["baseline", "candidate"] if round_id % 2 == 0 else ["candidate", "baseline"]
        for variant in order:
            name = f"keys-{keys}-round-{round_id}-{variant}"
            result_path = out / f"{name}.json"
            env = dict(os.environ, EDGE_H3_POOL_KEYS=str(keys), EDGE_H3_POOL_CYCLES=str(cycles),
                       EDGE_H3_POOL_RESULT=str(result_path))
            env.pop("EDGE_H3_TEST_OUTPUT", None)
            before = counters()
            with (out / f"{name}.log").open("w") as log:
                process = subprocess.run(["taskset", "-c", str(cpu), str(out / f"{variant}-tests"),
                                          "origin_h3::wire_tests::real_idle_pool_cost_probe",
                                          "--exact", "--ignored", "--test-threads=1", "--nocapture"],
                                         env=env, stdout=log, stderr=subprocess.STDOUT, timeout=210)
            observation = {"variant": variant, "keys": keys, "round": round_id,
                           "before": before, "after": counters(), "exit_code": process.returncode}
            (out / f"{name}-host.json").write_text(json.dumps(observation, indent=2) + "\n")
            process.check_returncode()
            result = json.loads(result_path.read_text())
            assert result["passed"] and result["keys"] == keys and result["closed_connections"] == keys
            assert result["cycles_per_block"] == cycles and len(result["block_ns"]) == 20
            assert all(value > 0 for value in result["block_ns"])
            rows.append({"variant": variant, "keys": keys, "round": round_id,
                         "ns_per_cycle": sum(result["block_ns"]) / (20 * cycles)})
summary = []
for keys in [1, 16, 256, 1024]:
    ratios = []
    for round_id in range(5):
        pair = {row["variant"]: row["ns_per_cycle"] for row in rows
                if row["keys"] == keys and row["round"] == round_id}
        ratios.append(pair["candidate"] / pair["baseline"])
    summary.append({"keys": keys, "candidate_over_baseline": ratios, "paired_median": statistics.median(ratios)})
for variant, expected in metadata["binaries"].items():
    assert hashlib.sha256((out / f"{variant}-tests").read_bytes()).hexdigest() == expected
(out / "summary.json").write_text(json.dumps({"rows": rows, "summary": summary}, indent=2) + "\n")
print(json.dumps(summary, indent=2))
