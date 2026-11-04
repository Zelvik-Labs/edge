"""Use the existing production profiler for H3; never mix into capacity data."""
import argparse
import importlib.util
from pathlib import Path

root=Path.cwd()
spec=importlib.util.spec_from_file_location('profile_driver',root/'scripts/perf/profile_architecture.py')
driver=importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)
original=driver.matrix.run
def run(args):
    args.protocols=['h3']
    args.experiment='instrumented H3 production CPU profile; exclude from capacity'
    return original(args)
driver.matrix.run=run
args=argparse.Namespace(output=root/'.local/h3-stage1/profiles-after',
    tls=root/'.local/perf-next/tls',client=root/'.local/protocol-state-matrix/protocol-state-client',
    edge_binary=Path('/workspace/edge/target/release/Zelvik-Edge'),
    controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
    profiler_image='edge-gap-profiler:20261101',frequency=199,seconds=15,cold_requests=2048,
    states=['hit','off'])
raise SystemExit(driver.main(args))
