import collections
import json
from pathlib import Path
import statistics

base=Path(__file__).resolve().parent
root=base/'udp/extracted'
cells=[]
all_resources=[]
cohorts=[]
for budget in [1,2]:
    directory=root/f'results-cpu{budget}/comparison'
    metadata=json.loads((directory/'metadata.json').read_text())
    records=json.loads((directory/'runs.json').read_text())
    summary=json.loads((directory/'summary.json').read_text())
    for case,cell in summary['comparisons'].items():
        c,b=map(int,[case.split('-')[0][1:],case.split('-')[1][1:]])
        rows=[r for r in records if r['concurrency']==c and r['payload']==b]
        result={'budget':budget,'case':case,'rps':{t:v['median'] for t,v in cell['rps'].items()},
                'p99_ns':{t:v['median'] for t,v in cell['p99_rtt_ns'].items()},
                'paired':cell['paired_changes_pct'],'resources':{}}
        for target in ['afxdp','kernel','nginx','openresty']:
            selected=sorted([r for r in rows if r['target']==target],key=lambda r:r['round'])
            values=collections.defaultdict(list)
            for r in selected:
                label=f"{case}-{target}-r{r['round']}"
                q=summary['resources'][label]
                count=r['result']['requests']
                hz=metadata['clock_ticks']
                values['rss_sum_bytes'].append(q['proxy_rss_bytes_sum_of_processes'])
                for field in ['Rss','Pss','Private_Clean','Private_Dirty','Anonymous','Swap']:
                    values['post_idle_smaps_'+field+'_sum_bytes'].append(sum(m['totals_bytes'][field] for m in r['memory_after_idle'].values()))
                values['process_cpu_us_per_timed_rtt_approx'].append(sum(
                    r['server_after']['processes'][pid]['ticks']-before['ticks']
                    for pid,before in r['server_before']['processes'].items())/hz*1e6/count)
                group=collections.defaultdict(lambda: {'user_ticks':0,'system_ticks':0,'tids':[]})
                unknown=[]
                for pid,delta in r['thread_cpu_deltas'].items():
                    for t in delta['threads']:
                        if t['status']!='matched':
                            unknown.append({'pid':pid,**t});continue
                        name=t['comm_before'] if t['comm_before']==t['comm_after'] else t['comm_before']+' -> '+t['comm_after']
                        group[name]['user_ticks']+=t['user_ticks']
                        group[name]['system_ticks']+=t['system_ticks']
                        group[name]['tids'].append({'pid':pid,'tid':t['tid']})
                for name,v in group.items():
                    v['user_us_per_timed_rtt']=v['user_ticks']/hz*1e6/count
                    v['system_us_per_timed_rtt']=v['system_ticks']/hz*1e6/count
                    v['cpu_us_per_timed_rtt']=v['user_us_per_timed_rtt']+v['system_us_per_timed_rtt']
                cohorts.append({'budget':budget,'case':label,'target':target,'concurrency':c,'payload':b,
                                'complete_identity_coverage':q['thread_cpu_complete_identity_coverage'],
                                'known_matched_by_comm':dict(group),'unknown_rows':unknown,
                                'scope':'matched native CPU counters; sequential read windows, not function attribution; unknown rows excluded from named matched sums but retained explicitly'})
            result['resources'][target]={k:{'all':v,'median':statistics.median(v)} for k,v in values.items()}
        cells.append(result)
    for label,q in summary['resources'].items():
        all_resources.append({'budget':budget,'case':label,**q})
quality={'cases':len(all_resources),'flags':dict(collections.Counter(f for r in all_resources for f in r['quality_flags'])),
         'duration_seconds_range':[min(r['duration_seconds'] for r in all_resources),max(r['duration_seconds'] for r in all_resources)],
         'max_snapshot_overhead_pct':max(r['snapshot_overhead_pct'] for r in all_resources),
         'max_observed_steal_pct':max(c['steal_pct'] for r in all_resources for c in r['per_cpu'].values()),
         'all_resources':all_resources}
for name,value in [('analysis.json',{'cells':cells}),('quality.json',quality),('thread-cohorts.json',cohorts)]:
    (base/name).write_text(json.dumps(value,indent=2)+'\n')
for c in cells:
    print(c['budget'],c['case'],{t:round(v['process_cpu_us_per_timed_rtt_approx']['median'],2) for t,v in c['resources'].items()},
          'RSS MiB',{t:round(v['rss_sum_bytes']['median']/2**20,2) for t,v in c['resources'].items()})
print('quality',{k:v for k,v in quality.items() if k!='all_resources'})
for budget in [1,2]:
    for c in [1,16]:
        by_name=collections.defaultdict(list)
        for row in cohorts:
            if row['budget']==budget and row['concurrency']==c and row['target']=='afxdp':
                for name,v in row['known_matched_by_comm'].items():
                    by_name[name].append([v['user_us_per_timed_rtt'],v['system_us_per_timed_rtt']])
        print('AF matched threads',budget,c,{name:[round(statistics.median(v[i] for v in values),3) for i in [0,1]] for name,values in by_name.items()})
