#!/usr/bin/env python3
"""Compile both real library test executables before any timing begins."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess

BASE = "b5bc67ee7484915d7d6f86319cf239ff5d8ade38"
root = Path.cwd()
out = root / ".local/h3-pool-ci"
out.mkdir(parents=True, exist_ok=True)
source = root / "src/origin_h3_pool.rs"
candidate = source.read_bytes()
baseline = subprocess.check_output(["git", "show", f"{BASE}:src/origin_h3_pool.rs"])
metadata = {"commit": os.environ["GITHUB_SHA"], "baseline_pool_commit": BASE,
            "scope": "only origin_h3_pool.rs differs; common real-H3 test fixture",
            "profile": "release library test: System allocator, unwind test panic policy",
            "rustflags": os.environ["RUSTFLAGS"], "variants": {}}
try:
    for variant, content in [("baseline", baseline), ("candidate", candidate)]:
        source.write_bytes(content)
        (out / f"{variant}-pool.rs").write_bytes(content)
        with (out / f"{variant}-build.jsonl").open("w") as stdout, (out / f"{variant}-build.log").open("w") as stderr:
            subprocess.run(["cargo", "test", "--locked", "--release", "--lib", "--no-run", "--message-format=json"],
                           stdout=stdout, stderr=stderr, check=True)
        artifacts = [json.loads(line) for line in (out / f"{variant}-build.jsonl").read_text().splitlines()]
        bins = [p["executable"] for p in artifacts if p.get("reason") == "compiler-artifact"
                and p.get("executable") and p["target"]["name"] == "zelvik_edge" and p["profile"]["test"]]
        assert len(bins) == 1, bins
        binary = out / f"{variant}-tests"
        shutil.copy2(bins[0], binary)
        metadata["variants"][variant] = {
            "pool_sha256": hashlib.sha256(content).hexdigest(),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        }
finally:
    source.write_bytes(candidate)
    assert source.read_bytes() == candidate
    (out / "build-metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
subprocess.run(["git", "diff", "--exit-code", "--", "src", "Cargo.toml", "Cargo.lock"], check=True)
with (out / "candidate-tests.log").open("w") as log:
    subprocess.run([str(out / "candidate-tests"), "origin_h3", "--test-threads=2", "--nocapture"],
                   stdout=log, stderr=subprocess.STDOUT, check=True)
