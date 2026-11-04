#!/usr/bin/env python3
"""Verify retained ZIP bytes/files and recompute successful component summaries."""
import hashlib,json,math,statistics,sys,zipfile
from pathlib import Path
root=Path(sys.argv[1]) if len(sys.argv)>1 else Path(__file__).parent
for manifest in sorted(root.rglob('manifest.json')):
 meta=json.loads(manifest.read_text());archive=manifest.with_name('raw.zip')
 assert hashlib.sha256(archive.read_bytes()).hexdigest()==meta['archive_sha256']
 with zipfile.ZipFile(archive) as z:
  for item in meta['files']:
   value=z.read(item['path']);assert len(value)==item['bytes'];assert hashlib.sha256(value).hexdigest()==item['sha256']
  if 'summary.json' in z.namelist():
   summary=json.loads(z.read('summary.json'))
   if 'rows' in summary:
    values={}
    for row in summary['rows']:
     k,r,v=row['keys'],row['round'],row['variant'];raw=json.loads(z.read(f'keys-{k}-round-{r}-{v}.json'))
     value=sum(raw['block_ns'])/(len(raw['block_ns'])*raw['cycles_per_block'])
     assert math.isclose(value,row['ns_per_cycle'],rel_tol=1e-12)
     assert raw['passed'] and raw['closed_connections']==k and len(raw['block_ns'])==20
     values[k,r,v]=value
    assert len(values)==40
    for group in summary['summary']:
     ratios=[values[group['keys'],r,'candidate']/values[group['keys'],r,'baseline'] for r in range(5)]
     assert ratios==group['candidate_over_baseline'];assert statistics.median(ratios)==group['paired_median']
   elif 'medians' in summary:
    rows=json.loads(z.read('records.json'));assert len(rows)==30
    for cap,pair in summary['medians'].items():
     for variant,fields in pair.items():
      matches=[r for r in rows if r['variant']==variant and r['capacity_requested']==int(cap)];assert len(matches)==5
      for field,value in fields.items():assert statistics.median(r[field] for r in matches)==value
    build=json.loads(z.read('build.json'))
    for variant,identity in build['variants'].items():
     assert hashlib.sha256(z.read(f'{variant}-probe')).hexdigest()==identity['binary_sha256']
     assert hashlib.sha256(z.read(f'{variant}-source.rs')).hexdigest()==identity['source_sha256']
 print(manifest.parent, 'archive/files/arithmetic OK')
