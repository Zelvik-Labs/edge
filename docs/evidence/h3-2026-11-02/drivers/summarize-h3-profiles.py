"""Classify actual crate frames, excluding crate names inside generic arguments."""
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil

local=Path('.local/h3-stage1')
target=local/'profiles-after-summary.json'
original=local/'profiles-after-summary-original.json'
if not original.exists():shutil.copy2(target,original)
spec=importlib.util.spec_from_file_location('summary','scripts/perf/summarize_architecture.py')
summary=importlib.util.module_from_spec(spec);spec.loader.exec_module(summary)
roots={'quic_demux':'zelvik_edge::quic_udp_demux::','quinn_io':'quinn::',
    'quinn_protocol':'quinn_proto::','h3_protocol':'h3::','h3_quinn_adapter':'h3_quinn::',
    'h3_adapter':'zelvik_edge::h3_downstream::','memory_governor':'zelvik_edge::memory_governor::',
    'mpsc':'tokio::sync::mpsc::'}
patterns={name:re.compile(r'^\s*[0-9a-f]+\s+<?'+re.escape(prefix),re.M)
          for name,prefix in roots.items()}
assert not patterns['h3_protocol'].search('   abcd <pingora_proxy::HttpProxy<origin_h3::Connector>>::process')
assert patterns['h3_protocol'].search('   abcd <h3::server::RequestStream<S>>::send_response')
profiles=[]
manifest=json.loads((local/'profiles-after/manifest.json').read_text())
for case in manifest['cases']:
    path=local/'profiles-after'/(case['stem']+'.stacks.txt')
    p=summary.profile(path);p['state']=case['cache']
    blocks=[re.sub(r'\[[0-9a-f]{8,}\]','',b) for b in path.read_text().split('\n\n') if 'cpu-clock:u:' in b]
    for name,pattern in patterns.items():
        count=sum(bool(pattern.search(b)) for b in blocks)
        p['groups'][name]={'samples':count,'percent':count*100/len(blocks)}
    snapshot=re.compile(r'^\s*[0-9a-f]+\s+<zelvik_edge::memory_governor::MemoryGovernor>::snapshot',re.M)
    count=sum(bool(snapshot.search(b)) for b in blocks)
    p['groups']['governor_snapshot']={'samples':count,'percent':count*100/len(blocks)}
    count=sum(bool(snapshot.search(b) and patterns['quic_demux'].search(b)) for b in blocks)
    p['groups']['governor_snapshot_in_demux']={'samples':count,'percent':count*100/len(blocks)}
    profiles.append(p)
target.write_text(json.dumps({'diagnostic_rps_excluded':True,'inclusive_groups_overlap':True,
    'crate_group_frame_patterns':{k:p.pattern for k,p in patterns.items()},
    'classification_note':'Crate groups match the function frame root, not crate names embedded in generic arguments. This prevents origin_h3::OriginH3Connector in Pingora generic types from being misclassified as h3 protocol work.',
    'original_broad_classifier_summary_sha256':hashlib.sha256(original.read_bytes()).hexdigest(),
    'profiles':profiles},indent=2)+'\n')
print([(p['state'],{k:round(p['groups'][k]['percent'],2) for k in patterns}) for p in profiles])
