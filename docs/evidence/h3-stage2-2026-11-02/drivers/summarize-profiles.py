"""Classify actual frame roots in before/after user CPU stacks."""
import importlib.util,json,re
from pathlib import Path
local=Path('.local/h3-stage2')
spec=importlib.util.spec_from_file_location('summary','scripts/perf/summarize_architecture.py');summary=importlib.util.module_from_spec(spec);spec.loader.exec_module(summary)
roots={'quic_demux':'zelvik_edge::quic_udp_demux::','quinn_io':'quinn::','quinn_protocol':'quinn_proto::','h3_protocol':'h3::','h3_quinn_adapter':'h3_quinn::','h3_adapter':'zelvik_edge::h3_downstream::','memory_governor':'zelvik_edge::memory_governor::','l4_defense':'zelvik_edge::l4_defense::','mpsc':'tokio::sync::mpsc::'}
patterns={name:re.compile(r'^\s*[0-9a-f]+\s+<?'+re.escape(prefix),re.M) for name,prefix in roots.items()}
snapshot=re.compile(r'^\s*[0-9a-f]+\s+<zelvik_edge::memory_governor::MemoryGovernor>::snapshot',re.M)
assert not patterns['h3_protocol'].search('  abcd <pingora_proxy::HttpProxy<origin_h3::Connector>>::process')
profiles=[]
for phase in ['before','after']:
    manifest=json.loads((local/('profiles-'+phase)/'manifest.json').read_text())
    for case in manifest['cases']:
        path=local/('profiles-'+phase)/(case['stem']+'.stacks.txt');p=summary.profile(path);p.update(phase=phase,state=case['cache'])
        blocks=[re.sub(r'\[[0-9a-f]{8,}\]','',b) for b in path.read_text().split('\n\n') if 'cpu-clock:u:' in b]
        for name,pattern in patterns.items():
            count=sum(bool(pattern.search(b)) for b in blocks);p['groups'][name]={'samples':count,'percent':count*100/len(blocks)}
        for name,condition in [('governor_snapshot',lambda b:snapshot.search(b)),('governor_snapshot_in_demux',lambda b:snapshot.search(b) and patterns['quic_demux'].search(b))]:
            count=sum(bool(condition(b)) for b in blocks);p['groups'][name]={'samples':count,'percent':count*100/len(blocks)}
        profiles.append(p)
(local/'profiles-summary.json').write_text(json.dumps({'capacity_excluded':True,'inclusive_groups_overlap':True,'frame_root_patterns':{k:v.pattern for k,v in patterns.items()},'profiles':profiles},indent=2)+'\n')
print([(p['phase'],p['state'],p['groups']['governor_snapshot_in_demux']) for p in profiles])
