import argparse
import subprocess
import json
import importlib.util
from pathlib import Path

root = Path.cwd()
local = root / '.local/h3-stage1'
spec = importlib.util.spec_from_file_location('matrix', root / 'scripts/perf/protocol_state_matrix.py')
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)
original = matrix.docker
target = None
def docker(*args):
    global target
    args = list(args)
    if args[0] == 'run' and 'RUST_LOG=warn' in args:
        args[args.index('RUST_LOG=warn')] = 'RUST_LOG=warn,zelvik_edge::http3_proxy_manager=debug,quinn_proto::endpoint=trace,quinn::endpoint=debug'
    if args[0] == 'run' and '/tools/controller' in args:
        target = args[args.index('--name')+1]
        args[args.index(matrix.IMAGES['edge'])] = 'edge-gap-profiler:20261101'
        args[1:1] = ['--cap-add', 'SYS_PTRACE', '--security-opt', 'seccomp=unconfined']
    return original(*args)
matrix.docker = docker
original_client = matrix.run_client
def run_client(args, protocol, state, connection, url, ca, stem):
    if stem.name == 'origin-reference':
        return original_client(args, protocol, state, connection, url, ca, stem)
    ready = json.loads((stem.parent/'edge-ready.json').read_text())
    (stem.parent/'udp-before.txt').write_text(original('exec',target,'cat','/proc/net/snmp'))
    recorder = subprocess.Popen([*matrix.DOCKER, 'exec', target, 'strace', '-ff', '-tt', '-T',
        '-e', 'trace=network', '-o', '/test/server-strace', '-p', str(ready['edge_pid'])],
        env=matrix.ENV, stdout=subprocess.DEVNULL, stderr=(stem.parent/'strace-attach.log').open('w'))
    try:
        return original_client(args, protocol, state, connection, url, ca, stem)
    finally:
        original('exec',target,'pkill','-INT','-x','strace')
        recorder.wait(timeout=10)
        (stem.parent/'udp-after.txt').write_text(original('exec',target,'cat','/proc/net/snmp'))
matrix.run_client = run_client
args = argparse.Namespace(target='edge', output=local/'fresh-server-strace',
    tls=root/'.local/perf-next/tls', client=local/'strace-client',
    edge_binary=local/'baseline-Zelvik-Edge',
    controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
    seconds=3, repeats=1, payload_size=1024, concurrency=32, cold_requests=2048,
    fresh_requests=256, protocols=['h3'], states=['off'], connections=['fresh'],
    multiplex=False, seed=20261102, idle_reference_seconds=2,
    experiment='diagnostic only: verbose client and H3 accept log; exclude capacity')
raise SystemExit(matrix.run(args))
