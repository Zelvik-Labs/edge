"""Collect before/after H3 CPU profiles outside capacity runs."""
import argparse,hashlib,importlib.util,json
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2'
spec=importlib.util.spec_from_file_location('profile_driver',root/'scripts/perf/profile_architecture.py')
driver=importlib.util.module_from_spec(spec);spec.loader.exec_module(driver)
original=driver.matrix.run
def run(args):
    args.protocols=['h3'];args.experiment='instrumented H3 CPU profile; excluded from capacity'
    return original(args)
driver.matrix.run=run
original_write=driver.matrix.write
def record_command(path,data):
    if str(path).endswith('.perf-command.json'):
        pos=data.index('record')+1
        data[pos:pos]=['-m','16M','-C','0,1']
    return original_write(path,data)
driver.matrix.write=record_command

original_docker=driver.matrix.docker
original_client=driver.matrix.run_client
quality=json.loads((local/'initial-profile-quality.json').read_text())
for item in quality['profiles']:
    path=Path(item['file']);item['sha256']=hashlib.sha256(path.read_bytes()).hexdigest()
    item['file']=str(path.parent.with_name(path.parent.name+'-initial')/path.name)
(local/'initial-profile-quality.json').write_text(json.dumps(quality,indent=2)+'\n')
for phase in ['before','after']:
    (local/('profiles-'+phase)).rename(local/('profiles-'+phase+'-initial'))
for name,binary in [('before',local/'baseline-Zelvik-Edge'),('after',Path('/workspace/edge/target/release/Zelvik-Edge'))]:
    args=argparse.Namespace(output=local/('profiles-'+name),tls=root/'.local/perf-next/tls',
        client=root/'.local/protocol-state-matrix/protocol-state-client',edge_binary=binary,
        controller=Path('/workspace/edge/target/release/examples/issue34-controller'),
        profiler_image='edge-gap-profiler:20261101',frequency=99,seconds=15,cold_requests=2048,states=['hit','off'])
    code=driver.main(args)
    driver.matrix.docker=original_docker
    driver.matrix.run_client=original_client
    if code:raise SystemExit(code)
