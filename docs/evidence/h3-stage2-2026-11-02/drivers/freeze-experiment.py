"""Keep the measured candidate reproducible while the next experiment changes source."""
import hashlib,json,shutil
from pathlib import Path
root=Path.cwd();local=root/'.local/h3-stage2';path=local/'build-metadata.json';m=json.loads(path.read_text())
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
snapshot=local/'measured-source';snapshot.mkdir(exist_ok=False)
for name,digest in m['source_sha256'].items():
    source=root/name;assert sha(source)==digest,name
    dest=snapshot/name;dest.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(source,dest)
m['source_snapshot_root']=str(snapshot)
frozen=local/'frozen-binaries';frozen.mkdir(exist_ok=False)
for name,entry in m['binaries'].items():
    source=Path(entry['path']);assert sha(source)==entry['sha256'],name
    dest=frozen/name;shutil.copy2(source,dest);entry.update(measured_path=entry['path'],path=str(dest))
source=Path(m['microbenchmark']['path']);dest=frozen/'microbenchmark';shutil.copy2(source,dest)
m['microbenchmark'].update(measured_path=str(source),path=str(dest),frozen_path=str(dest))
path.write_text(json.dumps(m,indent=2)+'\n')
print({'frozen_source_files':len(m['source_sha256']),'binaries':len(m['binaries'])+1})
