#!/usr/bin/env python3
"""Same feature-enabled production binary, H3 transport attribution matrix."""
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
ROOT = Path.cwd()
OUT = ROOT / '.local/http-state-ci'
binary = ROOT / 'target/release/Zelvik-Edge'
digest = hashlib.sha256(binary.read_bytes()).hexdigest()
common = ['--target','edge','--tls',str(OUT/'tls'),'--client',str(OUT/'protocol-state-client'),
          '--edge-binary',str(binary),'--controller',str(ROOT/'target/release/examples/issue34-controller'),
          '--edge-image','edge-http-state-fixture:20261103','--protocols','h3','--seconds','3',
          '--cold-requests','1024','--fresh-requests','512','--repeats','1','--multiplex',
          '--transport-profile','matched','--address-validation','off']
backends = ['quinn-shared','quinn-native','tokio-quiche-native']
metadata = {'source':os.environ['GITHUB_SHA'],'binary_sha256':digest,
            'scope':'same complete CDN/cache node, matched transport; explicit Retry off; no native UDP multiplexing or XDP',
            'profile':'256KiB receive/send;64KiB stream;MTU1350;uni8;idle30s;Cubic initial13500;PMTU off;migration off;quiche RX<=1500/GRO off/GSO on;TLS providers differ', 'runs':[]}
records=[]
def save():
    (OUT/'native-suite.json').write_text(json.dumps(metadata,indent=2)+'\n')
    (OUT/'native-records.json').write_text(json.dumps(records,indent=2)+'\n')
for repeat in range(3):
    order=backends[repeat:]+backends[:repeat]
    for size in [1024,32768,131072]:
        for backend in order:
            label=f'native-{repeat}-{backend}-{size}'
            directory=OUT/label
            with (OUT/f'{label}.log').open('w') as log:
                process=subprocess.run([sys.executable,'scripts/perf/protocol_state_matrix.py',
                    '--output',str(directory),'--payload-size',str(size),'--edge-backend',backend,*common],
                    stdout=log,stderr=subprocess.STDOUT)
            metadata['runs'].append({'label':label,'exit_code':process.returncode,'backend':backend,'size':size,'round':repeat})
            save()
            path=directory/'manifest.json'
            if not path.exists():
                print((OUT/f'{label}.log').read_text()[-6000:],file=sys.stderr)
                raise RuntimeError(f'fixture did not create a manifest: {label}')
            manifest=json.loads(path.read_text())
            if manifest['status']!='completed' or manifest.get('cleanup_errors'):
                raise RuntimeError(f'fixture/cleanup failure: {label}; {manifest.get("error")}')
            assert manifest['source_revision']==metadata['source']
            assert manifest['args']['edge_backend']==backend
            assert manifest['args']['transport_profile']=='matched' and manifest['args']['address_validation']=='off'
            assert len(manifest['cases'])==9
            for case in manifest['cases']:
                records.append({'backend':backend,'bytes':size,'round':repeat,'cache':case['cache'],
                    'connection':case['connection'],'status':case['status'],'native':case.get('native'),
                    'cpu_usec_per_request':case.get('proxy_cpu_usec_per_200'),
                    'memory_peak':case.get('proxy_after',{}).get('memory.peak'),
                    'origin_requests':case.get('origin_requests'),'manifest':str(path.relative_to(OUT))})
            save()
assert hashlib.sha256(binary.read_bytes()).hexdigest()==digest
lines=['# Matched H3 transport comparison','',f"Source `{metadata['source']}`. Same feature-enabled production executable.",
       'Three alternating rounds per cell; cache/body/TLS/protocol gates remain enabled. No long-soak claim.','',
       '| Bytes | Cache | Connection | Shared Quinn RPS | Native Quinn RPS | Native tokio-quiche RPS |',
       '|---:|---|---|---:|---:|---:|']
for size in [1024,32768,131072]:
    for cache in ['off','miss','hit']:
        for connection in ['reuse','fresh','multiplex']:
            cells=[]
            for backend in backends:
                rows=[r for r in records if (r['backend'],r['bytes'],r['cache'],r['connection'])==(backend,size,cache,connection)]
                assert len(rows)==3
                cells.append(f"{statistics.median(r['native']['good_rps'] for r in rows):.1f}" if all(r['status']=='passed' for r in rows) else 'FAILED (raw retained)')
            lines.append('| '+' | '.join(map(str,[size,cache,connection,*cells]))+' |')
(OUT/'native-comparison.md').write_text('\n'.join(lines)+'\n')
if any(run['exit_code'] for run in metadata['runs']): raise SystemExit('native comparison includes failures')
