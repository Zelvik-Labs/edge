import json
import subprocess
import sys
import unittest
from thread_cpu import compare, parse_stat, snapshot


def fixture():
    def value(tid, tick, start, when):
        return {'pid':tid,'comm': 'worker(name)', 'user_ticks':tick,'system_ticks':0,'start_ticks':start,
                'read_before_ns':when,'read_after_ns':when+1}
    before={'pid':42,'clock_ticks':100,'process_begin':value(42,10,1,100),'process_end':value(42,11,1,110),
            'threads':{'42':value(42,10,1,105)},'missing':[]}
    after={'pid':42,'clock_ticks':100,'process_begin':value(42,20,1,200),'process_end':value(42,22,1,210),
           'threads':{'42':value(42,20,1,205)},'missing':[]}
    return before,after


class CounterTests(unittest.TestCase):
    def test_stat_name_can_include_spaces_and_parentheses(self):
        fields=['S']+['0']*49
        fields[11],fields[12],fields[19]='30','7','12345'
        value=parse_stat('42 (weird ) thread(name)) '+' '.join(fields))
        self.assertEqual(value['comm'],'weird ) thread(name)')
        self.assertEqual((value['user_ticks'],value['system_ticks'],value['start_ticks']),(30,7,12345))

    def test_matching_counters_preserve_read_skew_bounds(self):
        before,after=fixture();result=compare(before,after)
        self.assertTrue(result['complete_identity_coverage'])
        self.assertEqual(result['threads'][0]['cpu_ticks'],10)
        self.assertEqual(result['process_tick_delta_bounds'],[9,12])

    def test_new_exited_and_reused_thread_ids_are_unobserved_not_zero(self):
        before,after=fixture()
        before['threads']['50']=dict(before['threads']['42'],pid=50)
        after['threads']['51']=dict(after['threads']['42'],pid=51)
        after['threads']['42']['start_ticks']=2
        result=compare(before,after)
        self.assertFalse(result['complete_identity_coverage'])
        self.assertEqual([v['status'] for v in result['threads']],['identity_changed','no_longer_observed','newly_observed'])
        self.assertTrue(all(v['cpu_ticks'] is None for v in result['threads']))
        self.assertIsNone(result['matched_thread_tick_sum'])

    def test_disappearance_during_read_remains_visible(self):
        before,after=fixture();before['missing']=[{'tid':70,'reason':'thread disappeared during snapshot'}]
        result=compare(before,after)
        self.assertFalse(result['complete_identity_coverage'])
        self.assertEqual(result['missing_during_snapshots'],before['missing'])

    def test_invalid_counter_or_process_identity_fails(self):
        before,after=fixture()
        after['threads']['42']['user_ticks']=1
        with self.assertRaisesRegex(AssertionError,'decreased'):compare(before,after)
        before,after=fixture();after['process_end']['start_ticks']=2
        with self.assertRaisesRegex(AssertionError,'identity'):compare(before,after)

    def test_native_threads_have_measured_cpu_and_stable_identity(self):
        program=r'''
import ctypes,hashlib,json,sys,threading,time
libc=ctypes.CDLL(None,use_errno=True)
ready=threading.Barrier(3);begin=threading.Event();done=threading.Barrier(3);release=threading.Event()
def work(name):
    assert libc.prctl(15,name.encode(),0,0,0)==0
    ready.wait();begin.wait();target=time.thread_time_ns()+100_000_000
    while time.thread_time_ns()<target:hashlib.pbkdf2_hmac('sha256',b'synthetic',b'fixed',10000)
    done.wait();release.wait()
a=threading.Thread(target=work,args=('cpu(one)',));b=threading.Thread(target=work,args=('cpu(two)',));a.start();b.start()
ready.wait();print('READY',flush=True);assert sys.stdin.readline().strip()=='GO';begin.set();done.wait();print('DONE',flush=True)
assert sys.stdin.readline().strip()=='EXIT';release.set();a.join();b.join()
'''
        child=subprocess.Popen([sys.executable,'-c',program],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        try:
            self.assertEqual(child.stdout.readline().strip(),'READY')
            before=snapshot(child.pid);child.stdin.write('GO\n');child.stdin.flush()
            self.assertEqual(child.stdout.readline().strip(),'DONE')
            after=snapshot(child.pid);result=compare(before,after)
            rows=[v for v in result['threads'] if v.get('comm_after') in ['cpu(one)','cpu(two)']]
            self.assertEqual(len(rows),2)
            self.assertTrue(all(v['status']=='matched' and v['cpu_ticks']>0 for v in rows))
            self.assertTrue(result['complete_identity_coverage'])
            child.stdin.write('EXIT\n');child.stdin.flush();self.assertEqual(child.wait(timeout=10),0,child.stderr.read())
        finally:
            if child.poll() is None:child.kill();child.wait()
            for pipe in [child.stdin,child.stdout,child.stderr]:pipe.close()


    def test_native_thread_exit_and_creation_leave_unknown_deltas_visible(self):
        program=r''' 
import ctypes,json,sys,threading,time
libc=ctypes.CDLL(None,use_errno=True);old_ready=threading.Event();old_exit=threading.Event();new_ready=threading.Event();new_exit=threading.Event();ids={}
def old_worker():
    assert libc.prctl(15,b'old-worker',0,0,0)==0
    ids['old']=threading.get_native_id();old_ready.set();old_exit.wait()
def new_worker():
    assert libc.prctl(15,b'new-worker',0,0,0)==0
    ids['new']=threading.get_native_id();deadline=time.thread_time_ns()+50_000_000
    while time.thread_time_ns()<deadline:pass
    new_ready.set();new_exit.wait()
a=threading.Thread(target=old_worker);a.start();old_ready.wait();print(json.dumps(ids),flush=True)
assert sys.stdin.readline().strip()=='GO';old_exit.set();a.join()
b=threading.Thread(target=new_worker);b.start();new_ready.wait();print(json.dumps(ids),flush=True)
assert sys.stdin.readline().strip()=='EXIT';new_exit.set();b.join()
'''
        child=subprocess.Popen([sys.executable,'-c',program],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        try:
            old=json.loads(child.stdout.readline())['old'];before=snapshot(child.pid)
            child.stdin.write('GO\n');child.stdin.flush();ids=json.loads(child.stdout.readline())
            after=snapshot(child.pid);result=compare(before,after);rows={r['tid']:r for r in result['threads']}
            self.assertEqual(rows[old]['status'],'no_longer_observed')
            self.assertEqual(rows[ids['new']]['status'],'newly_observed')
            self.assertIsNone(rows[old]['cpu_ticks']);self.assertIsNone(rows[ids['new']]['cpu_ticks'])
            self.assertFalse(result['complete_identity_coverage'])
            self.assertGreater(result['process_tick_delta_bounds'][1],0)
            child.stdin.write('EXIT\n');child.stdin.flush();self.assertEqual(child.wait(timeout=10),0,child.stderr.read())
        finally:
            if child.poll() is None:child.kill();child.wait()
            for pipe in [child.stdin,child.stdout,child.stderr]:pipe.close()


if __name__=='__main__':unittest.main()
