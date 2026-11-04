#!/usr/bin/env python3
"""Sample the actual production Edge binary using the independent H1 fixture.

These instrumented runs diagnose CPU cost; they are not capacity results.
Raw perf.data (which includes stack memory) stays under the ignored output tree.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess


spec = importlib.util.spec_from_file_location(
    'matrix', Path(__file__).with_name('protocol_state_matrix.py'))
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)


def main(options):
    original_docker = matrix.docker
    original_client = matrix.run_client
    target = None
    matrix.IMAGES[options.target] = options.profiler_image

    def docker(*args):
        nonlocal target
        args = list(args)
        if args[0] == 'run' and '--name' in args and options.profiler_image in args:
            target = args[args.index('--name') + 1]
            args[1:1] = ['--cap-add', 'PERFMON', '--cap-add', 'SYS_PTRACE',
                         '--security-opt', 'seccomp=unconfined']
        return original_docker(*args)

    def run_client(args, protocol, state, connection, url, ca, stem):
        if stem.name == 'origin-reference':
            return original_client(args, protocol, state, connection, url, ca, stem)
        if options.target == 'edge':
            ready = json.loads((stem.parent / 'edge-ready.json').read_text())
            pids = [ready['edge_pid']]
        else:
            raw = original_docker('exec', target, 'pgrep', '-f', '^nginx: (master|worker|cache)')
            assert raw.split() and all(value.isdigit() for value in raw.split()), raw
            pids = sorted(map(int, raw.split()))
        identity = original_docker('exec', target, 'sha256sum', f'/proc/{pids[0]}/exe').split()[0]
        if options.target != 'edge':
            executable = stem.parent / 'profile-executable'
            if not executable.exists():
                remote_path = original_docker('exec', target, 'readlink', f'/proc/{pids[0]}/exe')
                assert remote_path.startswith('/') and not remote_path.endswith(' (deleted)')
                subprocess.run([*matrix.DOCKER, 'cp', target + ':' + remote_path, str(executable)],
                               env=matrix.ENV, check=True)
            assert matrix.sha(executable) == identity
        matrix.write(Path(str(stem) + '.profile-target.json'),
                     {'target': options.target, 'container': target, 'container_pids': pids,
                      'executable_sha256': identity})
        remote = '/test/' + stem.name
        command = [*matrix.DOCKER, 'exec', target, 'perf', 'record', '-F',
                   str(options.frequency), '-e', 'cpu-clock:u', '--call-graph',
                   'dwarf,32768', '-p', ','.join(map(str, pids)), '-o', remote + '.perf.data',
                   '--', 'sleep', str(options.seconds)]
        matrix.write(Path(str(stem) + '.perf-command.json'), command)
        with Path(str(stem) + '.perf-record.log').open('x') as log:
            recorder = subprocess.Popen(command, env=matrix.ENV, stdout=log,
                                        stderr=subprocess.STDOUT)
            try:
                result = original_client(args, protocol, state, connection, url, ca, stem)
            finally:
                code = recorder.wait(timeout=options.seconds + 10)
            if code:
                raise RuntimeError('perf record failed; see instrumented run log')
        for label, flags in [('flat', ['--no-children', '--call-graph', 'none']),
                             ('callers', ['--children', '--call-graph', 'graph,0.5,caller'])]:
            try:
                report = original_docker('exec', target, 'perf', 'report', '--stdio',
                                         '--percent-limit', '0.2', *flags,
                                         '-i', remote + '.perf.data')
            except subprocess.CalledProcessError as error:
                Path(str(stem) + '.perf-' + label + '-error.txt').write_text(error.output or '')
                raise
            Path(str(stem) + '.perf-' + label + '.txt').write_text(report + '\n')
        stacks = original_docker('exec', target, 'perf', 'script', '-i', remote + '.perf.data')
        demangled = subprocess.check_output(['c++filt', '-s', 'rust'], input=stacks, text=True)
        Path(str(stem) + '.stacks.txt').write_text(demangled + '\n')
        return result

    matrix.docker = docker
    matrix.run_client = run_client
    args = argparse.Namespace(
        target=options.target, output=options.output, tls=options.tls, client=options.client,
        edge_binary=options.edge_binary, controller=options.controller,
        seconds=options.seconds, repeats=options.repeats, payload_size=options.payload_size, concurrency=32,
        cold_requests=None if options.timed_misses else options.cold_requests,
        fresh_requests=2048, protocols=options.protocols,
        states=options.states, connections=['reuse'], multiplex=False, seed=20261102,
        experiment='instrumented production CPU profile; exclude from capacity comparison',
        edge_image=options.profiler_image if options.target == 'edge' else options.edge_image,
        profiler_image=options.profiler_image,
        profile_driver_sha256=matrix.sha(__file__))
    return matrix.run(args)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', choices=['edge', 'nginx', 'openresty'], default='edge')
    parser.add_argument('--edge-image', default='edge-architecture-profiler:20261104')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--tls', type=Path, required=True)
    parser.add_argument('--client', type=Path, required=True)
    parser.add_argument('--edge-binary', type=Path, default=Path('/workspace/edge/target/release/Zelvik-Edge'))
    parser.add_argument('--controller', type=Path, default=Path('/workspace/edge/target/release/examples/issue34-controller'))
    parser.add_argument('--profiler-image', default='edge-gap-profiler:20261101')
    parser.add_argument('--frequency', type=int, default=199)
    parser.add_argument('--seconds', type=int, default=15)
    parser.add_argument('--cold-requests', type=int, default=8192)
    parser.add_argument('--timed-misses', action='store_true', help='Use the same timed client window for unique MISS requests')
    parser.add_argument('--repeats', type=int, default=1)
    parser.add_argument('--payload-size', type=int, default=1024)
    parser.add_argument('--protocols', nargs='+', choices=['h1', 'h1-tls', 'h2', 'h3'], default=['h1'])
    parser.add_argument('--states', nargs='+', choices=['off', 'hit', 'miss'], default=['off', 'hit', 'miss'])
    raise SystemExit(main(parser.parse_args()))
