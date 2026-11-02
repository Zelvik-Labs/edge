#!/usr/bin/env python3
"""Independent arithmetic and failure-retention checks for the result report."""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec=importlib.util.spec_from_file_location('summary',Path(__file__).with_name('summarize_protocol_state.py'))
summary=importlib.util.module_from_spec(spec);spec.loader.exec_module(summary)


class ReportTests(unittest.TestCase):
    def fixture(self,folder,target,multiplier):
        m={'target':target,'status':'completed','case_isolation':'synthetic fresh case fixture',
           'source_revision':'synthetic','driver_sha256':'synthetic','client_sha256':'synthetic',
           'client_source_sha256':'synthetic','args':{'payload_size':1024},
           'images':{target:'synthetic'},'server_version':'synthetic','scope':'synthetic',
           'cpu_assignment':{},'parent_limits':{},'client_versions':{},'started_at':0,'ended_at':10,'cases':[]}
        for repeat,rate in enumerate([100,300,200]):
            count=rate*multiplier*10
            m['cases'].append({'protocol':'h1','cache':'hit','connection':'reuse','repeat':repeat,
                'stem':str(repeat),'status':'passed','fresh_instance':True,'origin_requests':0,
                'integrity_before':{'cache_status':'HIT'},
                'native':{'good_rps':rate*multiplier,'successful':count,'failed':0,'p99_ms':repeat+1,
                          'new_connections':32,'request_limit':None,'error_distribution':{}},
                'elapsed':10,'client_cpu_seconds':6,'proxy_cpu_usec_per_200':5e6/count,
                'proxy_before':{'cpu.stat':{'usage_usec':0},'memory.events':{'oom_kill':0}},
                'proxy_after':{'cpu.stat':{'usage_usec':5000000},'memory.peak':{'bytes':16<<20},'memory.events':{'oom_kill':0}},
                'origin_before':{'cpu.stat':{'usage_usec':0}},'origin_after':{'cpu.stat':{'usage_usec':200000}},
                'host_cpu_before':{f'cpu{i}':[0]*8 for i in range(5)},
                'host_cpu_after':{f'cpu{i}':[100,0,0,800,0,0,0,100] for i in range(5)}})
        m['cases'].append({'protocol':'h3','cache':'hit','connection':'reuse','repeat':0,
                          'stem':'unsupported-state','status':'failed','fresh_instance':True,
                          'integrity_before':{'cache_status':'BYPASS'},'error':'actual X-Cache is BYPASS'})
        path=folder/(target+'.json');path.write_text(json.dumps(m));return path

    def test_medians_cpu_units_and_invalid_states_are_reported_without_zero_rps(self):
        with tempfile.TemporaryDirectory() as d:
            folder=Path(d)
            inputs=[self.fixture(folder,t,n) for t,n in [('edge',1),('nginx',2),('openresty',3)]]
            args=argparse.Namespace(inputs=inputs,evidence=folder/'evidence',report=folder/'report.md')
            summary.run(args)
            r=json.loads((args.evidence/'results.json').read_text())
            self.assertEqual((r['total_cases'],r['passed_cases']),(12,9))
            h1=next(g for g in r['groups'] if g['protocol']=='h1')
            self.assertEqual([h1['targets'][t]['good_rps_median'] for t in ['edge','nginx','openresty']],[200,400,600])
            self.assertAlmostEqual(h1['targets']['edge']['proxy_cpu_fraction_of_2cores_median'],.25)
            self.assertAlmostEqual(h1['targets']['edge']['client_cpu_fraction_adjusted_for_steal_median'],1/3)
            h3=next(g for g in r['groups'] if g['protocol']=='h3')
            self.assertIsNone(h3['targets']['edge']['good_rps_median'])
            self.assertIsNone(h3['targets']['edge']['failed'])
            self.assertEqual(h3['targets']['edge']['known_native_samples'],0)
            self.assertIn('BYPASS，状态不成立',args.report.read_text())
            self.assertIn('0.50×',args.report.read_text())

    def test_mixed_state_samples_cannot_enter_the_formal_comparison(self):
        with tempfile.TemporaryDirectory() as d:
            folder=Path(d);path=self.fixture(folder,'edge',1)
            m=json.loads(path.read_text());m['cases'][0]['fresh_instance']=False;path.write_text(json.dumps(m))
            args=argparse.Namespace(inputs=[path],evidence=folder/'evidence',report=folder/'report.md')
            with self.assertRaisesRegex(RuntimeError,'independently reset'):
                summary.run(args)

    def test_timeout_batches_cannot_be_hidden_by_an_available_throughput(self):
        with tempfile.TemporaryDirectory() as d:
            folder=Path(d);path=self.fixture(folder,'edge',1)
            m=json.loads(path.read_text())
            m['cases'][0]['status']='failed';m['cases'][0]['error']='execution timed out'
            del m['cases'][0]['native']
            path.write_text(json.dumps(m))
            args=argparse.Namespace(inputs=[path],evidence=folder/'evidence',report=folder/'report.md')
            summary.run(args)
            r=json.loads((args.evidence/'results.json').read_text())
            h1=next(g for g in r['groups'] if g['protocol']=='h1')['targets']['edge']
            self.assertEqual(h1['known_native_samples'],2)
            self.assertIsNone(h1['error_fraction'])
            self.assertEqual(h1['error_fraction_completed_batches'],0)
            self.assertIn('完整记录 2/3',args.report.read_text())


if __name__=='__main__':unittest.main()
