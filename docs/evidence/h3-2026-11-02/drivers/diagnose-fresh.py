import argparse
import importlib.util
from pathlib import Path

root = Path.cwd()
local = root / '.local/h3-stage1'
spec = importlib.util.spec_from_file_location('matrix', root / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)
original = matrix.docker
def docker(*args):
    args = list(args)
    if args[0] == 'run' and 'RUST_LOG=warn' in args:
        args[args.index('RUST_LOG=warn')] = 'RUST_LOG=warn,zelvik_edge::http3_proxy_manager=debug'
    return original(*args)
matrix.docker = docker
args = argparse.Namespace(target='edge', output=local/'fresh-diagnostic',
    tls=root/'.local/perf-next/tls', client=local/'diagnostic-client',
    edge_binary=local/'baseline-Zelvik-Edge',
    controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
    seconds=3, repeats=1, payload_size=1024, concurrency=32, cold_requests=2048,
    fresh_requests=256, protocols=['h3'], states=['off'], connections=['fresh'],
    multiplex=False, seed=20261102, idle_reference_seconds=2,
    experiment='diagnostic only: verbose client and H3 accept log; exclude capacity')
raise SystemExit(matrix.run(args))
