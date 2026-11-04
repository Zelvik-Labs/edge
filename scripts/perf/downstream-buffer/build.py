#!/usr/bin/env python3
"""Build the exact pre/post buffer production executables before measurement."""
import importlib.util
from pathlib import Path
ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('buffer_builder', ROOT / 'scripts/perf/cache-range/build.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)
builder.BASE = '4613a6b8d468be8df646f7cb628a42b89bfe0e98'
builder.CANDIDATE = '60cf30ced5f85f68b84f27e170972ca528115434'
builder.EXPECTED_RUNTIME_CHANGES = ['pingora-main/pingora-core/src/protocols/l4/stream.rs', 'src/http_proxy_manager.rs', 'src/memory_governor.rs']
builder.main()
