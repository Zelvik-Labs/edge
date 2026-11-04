#!/usr/bin/env python3
"""Compare cumulative runtime changes against the measured post-header baseline."""
import importlib.util
from pathlib import Path
ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('cumulative_builder', ROOT / 'scripts/perf/cache-range/build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
builder.BASE = 'ba474745fae3c08332d9e0862d9d5650550664ff'
builder.CANDIDATE = '59b9c849c2678b1c0f7aaae4ad0ffc23faeb5d9f'
builder.EXPECTED_RUNTIME_CHANGES = ['Cargo.toml', 'src/client_ip.rs', 'src/config.rs', 'src/lb_factory.rs', 'src/memory_governor.rs', 'src/memory_governor/snapshot_sequence.rs', 'src/metrics.rs', 'src/metrics/daily.rs', 'src/proxy.rs', 'src/quic_udp_demux.rs', 'src/udp_proxy.rs']
original_sha = builder.sha

def sha(path):
    # This module was added after the baseline; retain its explicit absence
    # rather than silently omitting an input or accepting missing executables.
    if path == ROOT / 'src/memory_governor/snapshot_sequence.rs' and not path.exists():
        assert builder.command('git', 'rev-parse', 'HEAD') == builder.BASE
        return None
    return original_sha(path)

builder.sha = sha
builder.main()
