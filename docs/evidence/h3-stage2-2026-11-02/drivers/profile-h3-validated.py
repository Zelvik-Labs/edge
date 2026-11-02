"""Repeat all four H3 profiles within the kernel's mmap allowance."""
import argparse
import importlib.util
from pathlib import Path

root=Path.cwd();local=root/'.local/h3-stage2'
spec=importlib.util.spec_from_file_location('profile_driver',root/'scripts/perf/profile_architecture.py')
driver=importlib.util.module_from_spec(spec);spec.loader.exec_module(driver)
original_run=driver.matrix.run
def run(args):
    args.protocols=['h3'];args.experiment='instrumented H3 profile retry: 99 Hz, 2 MiB mmap; excluded from capacity'
    return original_run(args)
driver.matrix.run=run
original_write=driver.matrix.write
def record_command(path,data):
    if str(path).endswith('.perf-command.json'):
        pos=data.index('record')+1
        data[pos:pos]=['-m','2M']
    return original_write(path,data)
driver.matrix.write=record_command
original_docker=driver.matrix.docker
original_client=driver.matrix.run_client
for phase in ['before','after']:
    path=local/('profiles-'+phase)
    if path.exists():path.rename(local/('profiles-'+phase+'-buffer-failed'))
for name,binary in [('before',local/'baseline-Zelvik-Edge'),('after',Path('/workspace/edge/target/release/Zelvik-Edge'))]:
    options=argparse.Namespace(output=local/('profiles-'+name),tls=root/'.local/perf-next/tls',client=root/'.local/protocol-state-matrix/protocol-state-client',edge_binary=binary,controller=Path('/workspace/edge/target/release/examples/issue34-controller'),profiler_image='edge-gap-profiler:20261101',frequency=99,seconds=15,cold_requests=2048,states=['hit','off'])
    code=driver.main(options)
    driver.matrix.docker=original_docker;driver.matrix.run_client=original_client
    if code:raise SystemExit(code)
