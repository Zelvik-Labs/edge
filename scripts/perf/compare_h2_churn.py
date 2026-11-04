#!/usr/bin/env python3
"""Diagnostic only: give Nginx the same 200-request H2 rotation boundary.

Uses the unchanged independent fixture/client; this is not part of the primary
capacity medians. No production Edge protection is disabled.
"""
import argparse
import importlib.util
from pathlib import Path

spec=importlib.util.spec_from_file_location('matrix',Path(__file__).with_name('protocol_state_matrix.py'))
matrix=importlib.util.module_from_spec(spec);spec.loader.exec_module(matrix)


def main(options):
    original=matrix.prepare
    def prepare(args,out,tls):
        payload=original(args,out,tls)
        path=out/'proxy.conf';text=path.read_text()
        assert text.count('keepalive_requests 1000000;')==1
        path.write_text(text.replace('keepalive_requests 1000000;','keepalive_requests 200;'))
        return payload
    matrix.prepare=prepare
    args=argparse.Namespace(target='nginx',output=options.output,tls=options.tls,client=options.client,
        edge_binary=Path('/workspace/edge/target/release/Zelvik-Edge'),
        controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
        seconds=options.seconds,repeats=1,payload_size=1024,concurrency=32,
        cold_requests=4096,fresh_requests=2048,protocols=['h2'],states=['off','hit'],
        connections=['reuse'],multiplex=True,seed=20261102,
        experiment='nginx keepalive_requests=200; diagnostic single repeat per case')
    return matrix.run(args)


if __name__=='__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--tls',type=Path,required=True)
    parser.add_argument('--client',type=Path,required=True)
    parser.add_argument('--seconds',type=int,default=8)
    raise SystemExit(main(parser.parse_args()))
