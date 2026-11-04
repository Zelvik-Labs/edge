#!/usr/bin/env python3
"""Build immutable nodes differing only in persisted-header validation."""
import importlib.util
from pathlib import Path

ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('header_node_builder', ROOT / 'scripts/perf/cache-range/build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
builder.BASE = 'a79b20d22c32e2b77c926e2335792b874b1d8bc8'
builder.CANDIDATE = 'ba474745fae3c08332d9e0862d9d5650550664ff'
builder.EXPECTED_RUNTIME_CHANGES = ['src/cache/mod.rs']
builder.main()
