#!/usr/bin/env python3
"""Build immutable nodes differing only in daily domain ownership and existing-key work."""
import importlib.util
from pathlib import Path

ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('daily_node_builder', ROOT / 'scripts/perf/cache-range/build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
builder.BASE = '739aa162d367040393e1d6d91293af231e7233e6'
builder.CANDIDATE = 'e3c1d50fa0c5300483102362b9de6beda8f3d0c5'
builder.EXPECTED_RUNTIME_CHANGES = ['src/metrics.rs', 'src/metrics/daily.rs']
builder.main()
