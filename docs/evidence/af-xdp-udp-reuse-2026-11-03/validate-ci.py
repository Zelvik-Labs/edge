import hashlib,json,math,statistics,sys,traceback
from pathlib import Path
root=Path(sys.argv[1]); out=Path(sys.argv[2])
meta=json.loads((root/'metadata.json').read_text())
assert meta['source_commit']=='4b29028edb47e6a038392d066eee747e2f34d3f7'
runs=json.loads((root/'runs.json').read_text()); summary=json.loads((root/'summary.json').read_text())
assert len(runs)==68
expected={(t,c,b,r) for c in [1,16] for b in [64,1200] for t in ['worker','readable','nginx','openresty'] for r in [1,2,3,4]}
expected|={('direct',c,b,0) for c in [1,16] for b in [64,1200]}
assert {(r['target'],r['concurrency'],r['payload'],r.get('round',0)) for r in runs}==expected
checked=[]; failures=[]
for record in runs:
 label=(f"direct-c{record['concurrency']}-b{record['payload']}" if record['target']=='direct' else f"r{record['round']}-c{record['concurrency']}-b{record['payload']}-{record['target']}")
 try:
  assert record['passed'] and not record['errors'] and record['pin_cleanup_ok'],record['errors']
  directory=root/label
  assert json.loads((directory/'record.json').read_text())==record
  data=json.loads((directory/'client.json').read_text()); assert data==record['result']
  origin=json.loads((directory/'origin.json').read_text()); assert origin==record['origin']
  samples=json.loads((directory/'samples.json').read_text()); count=record['concurrency']*meta['iterations']
  expected_sent=record['concurrency']*(meta['iterations']+meta['warmup'])
  assert data['passed'] and origin['passed'] and len(samples)==count and all(x>0 for x in samples)
  assert data['sent']==data['received']==origin['received']==origin['sent']==expected_sent
  assert origin['distinct_peer_ports']==record['concurrency']
  assert data['elapsed_ns']==data['timed_end_ns']-data['timed_start_ns']>0
  assert record['cpu_before']['monotonic_ns']<=data['timed_start_ns']<data['timed_end_ns']<=record['cpu_after']['monotonic_ns']
  ordered=sorted(samples)
  assert data['p50_rtt_ns']==ordered[(count-1)//2]
  assert data['p99_rtt_ns']==ordered[math.ceil(count*.99)-1]
  assert abs(data['mean_rtt_ns']-statistics.fmean(samples))<.001
  assert abs(data['requests_per_second']-count*1e9/data['elapsed_ns'])<.001
  if record['target'] in ['worker','readable']:
   node=record['node_report']; assert node['xskWakeMode']==record['target']
   assert node['proxyReady'] and node['proxyRedirectEnabled'] and node['redirect']>=expected_sent
   assert node['xskDrops']==0 and all(v==0 for v in node['xskQueueStats'].values())
   assert node['app']['udpDatagrams']==0,'remote origin must replace embedded backend'
  checked.append({'label':label,'rtt_samples':count,'validated_datagrams':expected_sent,'rps':data['requests_per_second'],'p99_rtt_ns':data['p99_rtt_ns']})
 except Exception:
  failures.append({'label':label,'error':traceback.format_exc()})
for c in [1,16]:
 for b in [64,1200]:
  key=f'c{c}-b{b}'; cell=summary['comparisons'][key]
  selected=[r for r in runs if r['concurrency']==c and r['payload']==b]
  assert cell['passed']==all(r['passed'] for r in selected)
  if cell['passed']:
   grouped={t:sorted([r for r in selected if r['target']==t],key=lambda r:r['round']) for t in ['worker','readable','nginx','openresty']}
   for t,rs in grouped.items():
    assert cell['rps'][t]['all']==[r['result']['requests_per_second'] for r in rs]
    assert cell['rps'][t]['median']==statistics.median(r['result']['requests_per_second'] for r in rs)
   for pair,metrics in cell['paired_changes_pct'].items():
    candidate,baseline=pair.split('_vs_')
    for metric,d in metrics.items():
     values=[(n['result'][metric]/o['result'][metric]-1)*100 for n,o in zip(grouped[candidate],grouped[baseline])]
     assert d['all']==values and d['median']==statistics.median(values)
assert summary['binaries_unchanged'] and summary['sources_unchanged']
result={'passed':not failures and summary['passed'],'source_commit':meta['source_commit'],'case_count':len(runs),'checked_cases':len(checked),'rtt_samples':sum(r['rtt_samples'] for r in checked),'failures':failures,'checks':checked,'scope':'independent recomputation from retained client, origin, record and RTT files; byte verification was performed by the pinned client, not reconstructed from RTTs'}
out.write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k!='checks'}))
sys.exit(0 if result['passed'] else 1)
