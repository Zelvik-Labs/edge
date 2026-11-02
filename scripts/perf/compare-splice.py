#!/usr/bin/env python3
"""Compare splice engines in isolated, alternating runs.

Build first, then run this script with no compiler running. This measures the
loopback relay component; it does not measure full Edge or HTTP capacity.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import time


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def cpu_stat():
    path = Path("/sys/fs/cgroup/cpu.stat")
    selected = {"cpu", *(f"cpu{cpu}" for cpu in os.sched_getaffinity(0))}
    proc = {parts[0]: [int(v) for v in parts[1:]] for line in Path("/proc/stat").read_text().splitlines() if (parts := line.split()) and parts[0] in selected}
    child = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {"cgroup": path.read_text() if path.exists() else None, "proc": proc, "child_user_seconds": child.ru_utime, "child_system_seconds": child.ru_stime}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--baseline-engine", choices=["blocking", "readiness", "uring"], default="blocking")
    parser.add_argument("--candidate-engine", choices=["blocking", "readiness", "uring"], default="readiness")
    parser.add_argument("--concurrency", type=int, choices=[1, 16], default=1)
    args = parser.parse_args()
    if args.rounds < 1:
        parser.error("rounds must be positive")
    if args.baseline_engine == args.candidate_engine:
        parser.error("baseline and candidate engines must differ")
    root = Path(__file__).resolve().parents[2]
    frozen = (root / "crates/zelvik-edge-io/benches/support/blocking.rs").read_text().split("mod original {", 1)[1].rsplit("}", 1)[0]
    normalized_hash = hashlib.sha256("".join(frozen.split()).encode()).hexdigest()
    if normalized_hash != "cd1f8753c1b37beb7cf2080c6aa45269257bcb4223196eaf7548f1cacc7225e6":
        raise SystemExit("frozen baseline splice changed; comparison requires a reviewed baseline")
    binary = args.binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cpus = sorted(os.sched_getaffinity(0))[:2]
    if len(cpus) != 2:
        parser.error("two allowed CPUs are required for the shared comparison baseline")
    # Children inherit this affinity, including Tokio's worker/blocking threads.
    os.sched_setaffinity(0, cpus)
    sources = [root / "Cargo.toml", root / "Cargo.lock", Path(__file__).resolve()]
    sources += sorted((root / "crates/zelvik-edge-io").rglob("*.rs"))
    sources.append(root / "crates/zelvik-edge-io/Cargo.toml")
    metadata = {
        "schema_version": 2,
        "scope": "single-process client + echo origin + relay component",
        "algorithm_reference_commits": {"blocking": "6f942440df52b7797fbb46cec86cef916681deef", "readiness": "2df8b242b1b0d24f199e22592b6924e592f960a2"},
        "baseline_engine": args.baseline_engine,
        "candidate_engine": args.candidate_engine,
        "concurrency": args.concurrency,
        "baseline_splice_normalized_sha256": normalized_hash,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
        "tracked_dirty": bool(subprocess.check_output(["git", "diff", "HEAD", "--name-only"], cwd=root, text=True).strip()),
        "kernel": platform.uname()._asdict(),
        "cpus": cpus,
        "binary_sha256": sha256(binary),
        "sources": {str(p.relative_to(root)): sha256(p) for p in sources},
        "cpu_max": Path("/sys/fs/cgroup/cpu.max").read_text().strip() if Path("/sys/fs/cgroup/cpu.max").exists() else None,
        "started_unix": time.time(),
        "rounds": args.rounds,
        "warmup_seconds": 1,
        "measurement_seconds": 3,
        "sample_size": 20,
    }
    write_json(output / "metadata.json", metadata)
    records = []
    failed = False
    for number in range(1, args.rounds + 1):
        engines = [args.baseline_engine, args.candidate_engine] if number % 2 else [args.candidate_engine, args.baseline_engine]
        for engine in engines:
            run_dir = output / f"round-{number}-{engine}"
            run_dir.mkdir()
            env = dict(os.environ, EDGE_SPLICE_BENCH_ENGINE=engine, EDGE_SPLICE_BENCH_CONCURRENCY=str(args.concurrency), CRITERION_HOME=str(run_dir / "criterion"))
            command = [str(binary), "--bench", "--noplot"]
            record = {"round": number, "engine": engine, "command": command, "cpu_before": cpu_stat(), "started_unix": time.time()}
            with (run_dir / "stdout.log").open("w") as stdout, (run_dir / "stderr.log").open("w") as stderr:
                try:
                    record["returncode"] = subprocess.run(command, cwd=root, env=env, stdout=stdout, stderr=stderr, timeout=180).returncode
                except subprocess.TimeoutExpired:
                    record.update(returncode=None, timeout_seconds=180)
            record["cpu_after"] = cpu_stat()
            record["finished_unix"] = time.time()
            estimates = sorted((run_dir / "criterion").glob("**/new/estimates.json"))
            record["cases"] = []
            for estimate in estimates:
                data = json.loads(estimate.read_text())
                mean = data["mean"]["point_estimate"]
                record["cases"].append({"bytes": int(estimate.parent.parent.name), "mean_ns": mean, "mean_per_roundtrip_ns": mean / args.concurrency, "roundtrips_per_second": args.concurrency * 1e9 / mean, "median_ns": data["median"]["point_estimate"], "mean_confidence_interval": data["mean"]["confidence_interval"]})
            valid_sizes = sorted(case["bytes"] for case in record["cases"]) == [1024, 65536, 1048576]
            record["passed"] = record["returncode"] == 0 and valid_sizes
            failed |= not record["passed"]
            records.append(record)
            write_json(run_dir / "result.json", record)
            write_json(output / "runs.json", records)
            print(f"round={number} engine={engine} passed={record['passed']} cases={len(record['cases'])}", flush=True)
    comparisons = []
    for size in [1024, 65536, 1048576]:
        paired = []
        for number in range(1, args.rounds + 1):
            values = {r["engine"]: next(c["mean_ns"] for c in r["cases"] if c["bytes"] == size) for r in records if r["round"] == number and r["passed"]}
            if len(values) == 2:
                paired.append({"round": number, **values, "change_percent": (values[args.candidate_engine] / values[args.baseline_engine] - 1) * 100})
        comparisons.append({"bytes": size, "pairs": paired, "median_percent": statistics.median(p["change_percent"] for p in paired) if paired else None})
    metadata.update(finished_unix=time.time(), binary_unchanged=sha256(binary) == metadata["binary_sha256"])
    metadata["sources_unchanged"] = all(sha256(root / path) == digest for path, digest in metadata["sources"].items())
    write_json(output / "metadata.json", metadata)
    write_json(output / "summary.json", {"all_passed": not failed and metadata["binary_unchanged"] and metadata["sources_unchanged"], "comparisons": comparisons})
    if failed or not metadata["binary_unchanged"] or not metadata["sources_unchanged"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
