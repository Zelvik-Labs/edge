#!/usr/bin/env python3
"""Build both table implementations, then measure fresh alternating processes."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import run

BASE = os.environ.get("WAF_BASELINE_REF") or "b5bc67ee7484915d7d6f86319cf239ff5d8ade38"
assert len(BASE) == 40 and all(c in '0123456789abcdef' for c in BASE), 'baseline must be an exact commit SHA'
ROOT = Path.cwd()
OUT = ROOT / ".local/waf-state-paired"
OUT.mkdir(parents=True, exist_ok=False)
source = ROOT / "src/firewall/bounded_map.rs"
candidate = source.read_bytes()
baseline = subprocess.check_output(["git", "show", f"{BASE}:src/firewall/bounded_map.rs"])
metadata = {"commit": os.environ["GITHUB_SHA"], "baseline": BASE, "variants": {},
            "scope": "same fixture; production table differs; System allocator component, not Node throughput"}
manifest = ROOT / "scripts/perf/waf-state/Cargo.toml"
try:
    for variant, content in [("baseline", baseline), ("candidate", candidate)]:
        source.write_bytes(content)
        (OUT / f"{variant}-source.rs").write_bytes(content)
        for stage, args in [("test", ["test", "--", "--nocapture"]),
                            ("clippy", ["clippy", "--all-targets", "--", "-D", "clippy::correctness", "-D", "clippy::suspicious"]),
                            ("build", ["build", "--release"])]:
            with (OUT / f"{variant}-{stage}.log").open("w") as log:
                subprocess.run(["cargo", args[0], "--locked", "--manifest-path", str(manifest), *args[1:]],
                               stdout=log, stderr=subprocess.STDOUT, check=True, timeout=600)
        binary = OUT / f"{variant}-probe"
        shutil.copy2(ROOT / "scripts/perf/waf-state/target/release/edge-waf-state-probe", binary)
        metadata["variants"][variant] = {"source_sha256": hashlib.sha256(content).hexdigest(),
                                        "binary_sha256": run.digest(binary)}
finally:
    source.write_bytes(candidate)
    assert source.read_bytes() == candidate
    run.write(OUT / "build.json", metadata)
subprocess.run(["git", "diff", "--exit-code", "--", "src", "scripts/perf/waf-state"], check=True)
allowed = sorted(os.sched_getaffinity(0))
os.sched_setaffinity(0, {allowed[0]})
run.write(OUT / "host.json", {"allowed": allowed, "pinned": allowed[0], "memory": run.memory(),
                             "rustflags": os.environ["RUSTFLAGS"], "page_size": os.sysconf("SC_PAGE_SIZE")})
rows = []
for capacity in [65_536, 1_677_721, 8_000_000]:
    for repeat in range(5):
        order = ["baseline", "candidate"] if repeat % 2 == 0 else ["candidate", "baseline"]
        for variant in order:
            run.compiler_guard()
            label = f"{capacity}-{repeat}-{variant}"
            before = run.cpu()
            result = subprocess.run([str(OUT / f"{variant}-probe"), str(capacity), str(repeat),
                                     str(os.sysconf("SC_PAGE_SIZE"))], capture_output=True, text=True, timeout=120)
            after = run.cpu()
            (OUT / f"{label}.stdout").write_text(result.stdout)
            (OUT / f"{label}.stderr").write_text(result.stderr)
            run.write(OUT / f"{label}-host.json", {"before": before, "after": after, "exit_code": result.returncode})
            result.check_returncode()
            row = json.loads(result.stdout)
            assert row["capacity_requested"] == capacity and row["repeat"] == repeat
            assert row["verified_hits"] == row["verified_misses"] == 200_000
            assert row["live_entries_after_clear"] == 0 and row["sparse_visits"] > 0
            assert row["rss_live_empty_delta_bytes"] == row["rss_empty"] - row["rss_before"]
            row["variant"] = variant
            rows.append(row)
            run.write(OUT / "records.json", rows)
fields = ["rss_live_empty_delta_bytes", "construction_ns", "empty_visit_ns", "empty_retain_ns",
          "empty_lookup_ns", "hit_lookup_ns", "rss_sparse", "sparse_fill_ns", "sparse_lookup_ns",
          "sparse_update_ns", "sparse_visit_ns", "clear_ns", "rss_after_clear"]
summary = {}
for capacity in [65_536, 1_677_721, 8_000_000]:
    summary[str(capacity)] = {variant: {field: statistics.median(
        r[field] for r in rows if r["capacity_requested"] == capacity and r["variant"] == variant)
        for field in fields} for variant in ["baseline", "candidate"]}
run.write(OUT / "summary.json", {"records": len(rows), "medians": summary})
for variant, item in metadata["variants"].items():
    assert run.digest(OUT / f"{variant}-probe") == item["binary_sha256"]
print(json.dumps(summary, indent=2))
