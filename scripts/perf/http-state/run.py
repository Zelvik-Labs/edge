#!/usr/bin/env python3
"""Sequential HTTP protocol/cache/reuse matrix and same-binary RX ablation."""
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys

ROOT = Path.cwd()
OUT = ROOT / ".local/http-state-ci"
common = ["--tls", str(OUT / "tls"), "--client", str(OUT / "protocol-state-client"),
          "--edge-binary", str(ROOT / "target/release/Zelvik-Edge"),
          "--controller", str(ROOT / "target/release/examples/issue34-controller"),
          "--edge-image", "edge-http-state-fixture:20261103", "--seconds", "3",
          "--cold-requests", "1024", "--fresh-requests", "512", "--repeats", "1", "--multiplex"]
binaries = [ROOT / "target/release/Zelvik-Edge", ROOT / "target/release/examples/issue34-controller",
            OUT / "protocol-state-client"]
hashes = {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in binaries}
metadata = {"source": os.environ["GITHUB_SHA"], "binary_sha256": hashes,
            "scope": "real production startup; one target/case at a time; two rounds, not long soak",
            "runs": []}
records = []

def save():
    (OUT / "suite.json").write_text(json.dumps(metadata, indent=2) + "\n")
    (OUT / "records.json").write_text(json.dumps(records, indent=2) + "\n")

for repeat in range(2):
    order = ["edge", "nginx", "openresty"] if repeat == 0 else ["openresty", "nginx", "edge"]
    for size in [1024, 32768, 131072]:
        for target in order:
            label = f"round-{repeat}-{target}-{size}"
            directory = OUT / label
            with (OUT / f"{label}.log").open("w") as log:
                process = subprocess.run([sys.executable, "scripts/perf/protocol_state_matrix.py",
                    "--target", target, "--output", str(directory), "--payload-size", str(size), *common],
                    stdout=log, stderr=subprocess.STDOUT)
            metadata["runs"].append({"label": label, "exit_code": process.returncode})
            manifest_path = directory / "manifest.json"
            if not manifest_path.exists():
                save()
                print((OUT / f"{label}.log").read_text()[-6000:], file=sys.stderr)
                raise RuntimeError(f"fixture failed before manifest: {label}; see retained log")
            manifest = json.loads(manifest_path.read_text())
            if manifest["status"] != "completed" or manifest.get("cleanup_errors"):
                save()
                raise RuntimeError(f"fixture/cleanup failed: {label}")
            for case in manifest["cases"]:
                records.append({"target": target, "size": size, "round": repeat,
                    "protocol": case["protocol"], "cache": case["cache"], "connection": case["connection"],
                    "status": case["status"], "reason": case.get("reason", case.get("error")),
                    "rps": case.get("native", {}).get("good_rps"), "p99_ms": case.get("native", {}).get("p99_ms"),
                    "errors": case.get("native", {}).get("failed"), "cpu_usec": case.get("proxy_cpu_usec_per_200"),
                    "memory_peak_bytes": case.get("proxy_after", {}).get("memory.peak", {}).get("bytes"),
                    "origin_requests": case.get("origin_requests"), "manifest": str(manifest_path.relative_to(OUT))})
            save()

# Keep the RX experiment separate from the default-target rankings.
for repeat in range(3):
    for batch in (["1", "32"] if repeat % 2 == 0 else ["32", "1"]):
        label = f"rx-round-{repeat}-batch-{batch}"
        with (OUT / f"{label}.log").open("w") as log:
            process = subprocess.run([sys.executable, "scripts/perf/protocol_state_matrix.py", "--target", "edge",
                "--output", str(OUT / label), "--payload-size", "1024", "--protocols", "h3",
                "--quic-rx-batch", batch, *common], stdout=log, stderr=subprocess.STDOUT)
        metadata["runs"].append({"label": label, "exit_code": process.returncode, "rx_batch": batch})
        save()

for path in binaries:
    assert hashlib.sha256(path.read_bytes()).hexdigest() == hashes[str(path.relative_to(ROOT))]
groups = {}
for row in records:
    key = (row["size"], row["protocol"], row["cache"], row["connection"])
    groups.setdefault(key, {}).setdefault(row["target"], []).append(row)
lines = ["# HTTP protocol / cache / connection comparison", "",
         f"Source: `{metadata['source']}`. Each cell is the median of two isolated runs.",
         "All failures remain in records.json; unsupported protocols have no invented throughput.", "",
         "| Bytes | Protocol | Cache | Connection | Edge RPS | Nginx RPS | OpenResty RPS |",
         "|---:|---|---|---|---:|---:|---:|"]
for key, targets in sorted(groups.items()):
    cells = []
    for target in ["edge", "nginx", "openresty"]:
        rows = targets[target]
        if all(row["status"] == "not-supported" for row in rows): cells.append("unsupported")
        elif len(rows) != 2 or any(row["status"] != "passed" for row in rows): cells.append("failed (see raw)")
        else: cells.append(f"{statistics.median(row['rps'] for row in rows):.1f}")
    lines.append("| " + " | ".join(map(str, [*key, *cells])) + " |")
(OUT / "comparison.md").write_text("\n".join(lines) + "\n")
save()
if any(run["exit_code"] for run in metadata["runs"]):
    raise SystemExit("one or more matrix runs failed; all attempts retained")
