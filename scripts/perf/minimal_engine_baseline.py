#!/usr/bin/env python3
"""H1 no-cache engine diagnostic using the independent production matrix fixture.

The minimal target has the same Pingora dependency, allocator and H1 session
loop, but no Edge configuration, accounting, WAF or cache. It is diagnostic
only, not a production Edge result. Other targets use their unchanged fixture.
"""
import argparse
import importlib.util
from pathlib import Path


spec = importlib.util.spec_from_file_location(
    'matrix', Path(__file__).with_name('protocol_state_matrix.py'))
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


def main(options):
    original_docker, original_write = matrix.docker, matrix.write
    matrix.IMAGES['minimal'] = matrix.IMAGES['edge']
    target = None

    def docker(*args):
        nonlocal target
        args = list(args)
        if options.target == 'minimal' and args[0] == 'run' and '/test/proxy.conf' in args:
            target = args[args.index('--name') + 1]
            image_index = args.index(matrix.IMAGES['minimal'])
            args = args[:image_index + 1] + ['--origin', 'origin:8081']
            args[args.index('--entrypoint') + 1] = '/tools/minimal'
            args[1:1] = ['--mount', f'type=bind,src={options.minimal_binary.resolve()},dst=/tools/minimal,readonly']
        if options.target == 'minimal' and args[:2] == ['exec', target] and args[2:] == ['nginx', '-V']:
            return 'minimal-http-proxy diagnostic; binary sha256=' + matrix.sha(options.minimal_binary)
        return original_docker(*args)

    matrix.docker = docker
    def write(path, data):
        if path.name == 'manifest.json' and data.get('target') == 'minimal':
            data['scope'] = 'diagnostic Pingora H1 loop; no production Edge filters or accounting'
        return original_write(path, data)
    matrix.write = write
    args = argparse.Namespace(
        target=options.target, output=options.output, tls=options.tls, client=options.client,
        edge_binary=options.edge_binary, controller=options.controller,
        seconds=options.seconds, repeats=options.repeats, payload_size=1024, concurrency=32,
        cold_requests=4096, fresh_requests=2048, protocols=['h1'],
        states=['off'], connections=['reuse'], multiplex=False, seed=20261102,
        experiment='minimal engine diagnostic; minimal target is not production Edge',
        minimal_binary_sha256=matrix.sha(options.minimal_binary),
        minimal_source_sha256=matrix.sha(matrix.ROOT / 'examples/minimal-http-proxy.rs'),
        diagnostic_driver_sha256=matrix.sha(__file__))
    return matrix.run(args)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=['edge', 'minimal', 'nginx', 'openresty'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tls', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--minimal-binary', type=Path, default=Path('/workspace/edge/target/release/examples/minimal-http-proxy'))
    parser.add_argument('--edge-binary', type=Path, default=Path('/workspace/edge/.worktrees/perf-next/.local/architecture-implementation/baseline-Zelvik-Edge'))
    parser.add_argument('--controller', type=Path, default=Path('/workspace/edge/target/release/examples/issue34-controller'))
    parser.add_argument('--seconds', type=int, default=8)
    parser.add_argument('--repeats', type=int, default=3)
    raise SystemExit(main(parser.parse_args()))
