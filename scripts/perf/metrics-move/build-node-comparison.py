#!/usr/bin/env python3
"""Build immutable nodes differing only in owned metric dimensions."""
import importlib.util
from pathlib import Path

ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('metrics_node_builder', ROOT / 'scripts/perf/cache-range/build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
builder.BASE = 'e36710ca5f231333ce3b388ea7dd4ad181c2fa69'
builder.CANDIDATE = 'e8d8b3abd57389e692e1777aa5910938efa0e160'
builder.EXPECTED_RUNTIME_CHANGES = ['src/metrics.rs']
builder.main()
