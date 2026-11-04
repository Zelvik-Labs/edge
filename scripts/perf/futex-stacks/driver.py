#!/usr/bin/env python3
"""Reuse the strict independent fixture with an operation/stack observer."""
import importlib.util
from pathlib import Path
ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('futex_driver', ROOT / 'scripts/perf/framework-syscalls/driver.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
if __name__ == '__main__':
    raise SystemExit(module.main(observer_helper=ROOT / 'scripts/perf/futex-stacks/helper.py',
        observer_image='edge-futex-sidecar:20261104',
        experiment='futex operation/caller diagnostics with checked thread coverage; all instrumented timing excluded'))
