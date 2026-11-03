import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

sys.path.insert(0, '/workspace/edge/.worktrees/perf-udp-owned/scripts/perf/afxdp-node')
from thread_cpu import snapshot, compare
from memory_maps import capture
from validate_resources import validate_resources

PROGRAM = r'''
import json,sys,threading,time
churn=sys.argv[1]=='churn'
old_ready=threading.Event();old_exit=threading.Event();new_done=threading.Event();release=threading.Event()
def old():old_ready.set();old_exit.wait()
def work():
 target=time.thread_time_ns()+70_000_000
 while time.thread_time_ns()<target:pass
 new_done.set();release.wait()
if churn:
 before=threading.Thread(target=old);before.start();old_ready.wait()
print('READY',flush=True);assert sys.stdin.readline().strip()=='GO'
start=time.monotonic_ns()
if churn:
 old_exit.set();before.join();after=threading.Thread(target=work);after.start();new_done.wait()
else:
 target=time.thread_time_ns()+70_000_000
 while time.thread_time_ns()<target:pass
end=time.monotonic_ns();print(json.dumps({'timed_start_ns':start,'timed_end_ns':end}),flush=True)
assert sys.stdin.readline().strip()=='EXIT'
if churn:release.set();after.join()
'''


class IndependentValidatorTests(unittest.TestCase):
    def exercise(self, churn):
        with tempfile.TemporaryDirectory() as name:
            path=Path(name)
            child=subprocess.Popen([sys.executable, '-c', PROGRAM, 'churn' if churn else 'stable'],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                self.assertEqual(child.stdout.readline().strip(),'READY')
                pid=str(child.pid)
                cpu_before=time.monotonic_ns();before=snapshot(child.pid)
                child.stdin.write('GO\n');child.stdin.flush()
                interval=json.loads(child.stdout.readline());after=snapshot(child.pid);cpu_after=time.monotonic_ns()
                delta=compare(before,after);idle=time.monotonic_ns();memory=capture(child.pid,path)
                record={'server_before':{'processes':{pid:{}}},'cpu_before':{'monotonic_ns':cpu_before},
                        'cpu_after':{'monotonic_ns':cpu_after},'result':interval,
                        'thread_cpu_before':{pid:before},'thread_cpu_after':{pid:after},
                        'thread_cpu_deltas':{pid:delta},'memory_after_idle':{pid:memory},
                        'idle_after':{'monotonic_ns':idle}}
                resources={'thread_cpu_complete_identity_coverage':not churn,
                           'quality_flags':['thread_cpu_identity_coverage_incomplete'] if churn else []}
                meta={'clock_ticks':before['clock_ticks']}
                validate_resources(record,meta,path,resources)
                self.assertEqual(delta['complete_identity_coverage'],not churn)
                if churn:
                    statuses={r['status'] for r in delta['threads']}
                    self.assertIn('newly_observed',statuses);self.assertIn('no_longer_observed',statuses)
                    for row in delta['threads']:
                        if row['status']!='matched':self.assertIsNone(row['cpu_ticks'])
                # Corrupt each independently recomputed source of evidence.
                for kind in ['cpu', 'rss', 'sha', 'coverage']:
                    bad=copy.deepcopy(record);bad_resources=copy.deepcopy(resources)
                    if kind=='cpu':
                        row=next(r for r in bad['thread_cpu_deltas'][pid]['threads'] if r['status']=='matched')
                        row['cpu_ticks']+=1
                    elif kind=='rss':bad['memory_after_idle'][pid]['totals_bytes']['Rss']+=4096
                    elif kind=='sha':bad['memory_after_idle'][pid]['raw_sha256']='0'*64
                    else:bad_resources['thread_cpu_complete_identity_coverage']=churn
                    with self.subTest(churn=churn,kind=kind), self.assertRaises(AssertionError):
                        validate_resources(bad,meta,path,bad_resources)
                child.stdin.write('EXIT\n');child.stdin.flush();self.assertEqual(child.wait(timeout=10),0,child.stderr.read())
            finally:
                if child.poll() is None:child.kill();child.wait()
                for pipe in [child.stdin,child.stdout,child.stderr]:pipe.close()

    def test_real_stable_child_and_corruption_controls(self):self.exercise(False)
    def test_real_churning_child_preserves_unknowns_and_corruption_controls(self):self.exercise(True)


if __name__=='__main__':unittest.main()
