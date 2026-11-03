#!/usr/bin/env python3
"""Independent IPv4 UDP reuse comparison: Edge wake modes, Nginx, OpenResty."""
import argparse
import hashlib
import json
import os
import platform
from pathlib import Path
import signal
import subprocess
import time
import traceback
from udp_summary import COMPARISON_NODES, summarize
from syscall_profile import SyscallProfile

ROOT = Path(__file__).resolve().parents[3]
NS = 'edge-udp-peer'


def write(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def command(*args):
    return subprocess.check_output([str(a) for a in args], text=True, stderr=subprocess.STDOUT)


def compiler_guard():
    for p in Path('/proc').glob('[0-9]*/comm'):
        try:
            if p.read_text().strip() in {'rustc', 'mbx-rustc', 'cc1', 'cc1plus', 'clang', 'ld.lld'}:
                state = next(s for s in p.with_name('status').read_text().splitlines() if s.startswith('State:'))
                if state.split()[1] != 'Z':
                    raise RuntimeError('comparison cannot overlap compilation')
        except FileNotFoundError:
            continue


def cpu():
    proc = {}
    for line in Path('/proc/stat').read_text().splitlines():
        p = line.split()
        if p and p[0].startswith('cpu'):
            proc[p[0]] = [int(x) for x in p[1:]]
    return {'monotonic_ns': time.monotonic_ns(), 'proc': proc}


def descendants(pid):
    found, pending = set(), [pid]
    while pending:
        current = pending.pop()
        if current in found:
            continue
        found.add(current)
        for task in Path(f'/proc/{current}/task').glob('*'):
            try:
                pending.extend(int(p) for p in (task/'children').read_text().split())
            except FileNotFoundError:
                continue
    return sorted(found)


def process_snapshot(pid):
    result = {'monotonic_ns': time.monotonic_ns(), 'processes': {}}
    for current in descendants(pid):
        root = Path(f'/proc/{current}')
        stat = (root/'stat').read_text().rsplit(')', 1)[1].split()
        threads = []
        for task in sorted((root/'task').glob('*')):
            fields = dict(line.split(':', 1) for line in (task/'status').read_text().splitlines() if ':' in line)
            threads.append({'tid': int(task.name), 'cpus': fields['Cpus_allowed_list'].strip()})
        result['processes'][str(current)] = {'ticks': int(stat[11]) + int(stat[12]),
            'rss_pages': int(stat[21]), 'fd_count': len(list((root/'fd').iterdir())), 'threads': threads}
    return result


def wait_file(path, process, timeout=30, as_json=False):
    deadline = time.monotonic() + timeout
    while True:
        if path.exists():
            if not as_json:
                return
            try:
                return json.loads(path.read_text())
            except json.JSONDecodeError:
                # A marker may be visible between create and buffered close.
                pass
        if process.poll() is not None:
            raise RuntimeError(f'process {process.pid} exited before {path.name}, code={process.returncode}')
        if time.monotonic() >= deadline:
            raise RuntimeError(f'timed out waiting for {path.name}')
        time.sleep(.002)


def stop_process(process):
    if process is None:
        return
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
    try:
        code = process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait(timeout=5)
        raise RuntimeError(f'process {process.pid} failed graceful shutdown')
    if code != 0:
        raise RuntimeError(f'process {process.pid} exited code={code}')


def json_reports(text):
    decoder, cursor, reports = json.JSONDecoder(), 0, []
    while (cursor := text.find('{', cursor)) >= 0:
        try:
            data, end = decoder.raw_decode(text[cursor:])
        except json.JSONDecodeError:
            cursor += 1
            continue
        cursor += end
        if isinstance(data, dict):
            reports.append(data)
    return reports


def configure(home, queue_cpu):
    (home/'configs').mkdir(parents=True)
    (home/'configs/api_node.yaml').write_text(f'''xdp:
  enabled: true
  attachMode: skb
  fallback: fail-start
  interfaces:
    - name: eup-node
      queues: [0]
      cpus: [{queue_cpu}]
      mode: proxy
      xskMode: copy
      localIps: [10.203.0.1]
      frameSize: 2048
  proxy:
    protocols: [http, https, tcp, udp, h3]
    ports:
      - {{protocol: http, port: 9080}}
      - {{protocol: https, port: 9444}}
      - {{protocol: tcp, port: 9443}}
      - {{protocol: udp, port: 443}}
      - {{protocol: h3, port: 8443}}
''')


def nginx_config(path, cpus):
    (path/'prefix').mkdir()
    masks = ' '.join(format(1 << c, 'b') for c in cpus)
    config = path/'nginx.conf'
    config.write_text(f'''worker_processes {len(cpus)};
worker_cpu_affinity {masks};
worker_rlimit_nofile 65536;
pid {path}/nginx.pid;
error_log {path}/server-error.log warn;
events {{ worker_connections 4096; }}
stream {{
  server {{
    listen 10.203.0.1:443 udp reuseport;
    proxy_pass 10.203.0.2:19003;
    proxy_requests 0;
    proxy_responses 2147483647;
    proxy_timeout 60s;
  }}
}}
''')
    return config


def fixed_network(path):
    # Node startup can tune NIC features. Apply the same software settings
    # afterwards for every target, then record actual values and queue counts.
    for namespace, interface in [(None, 'eup-node'), (NS, 'eup-peer')]:
        prefix = ['ip', 'netns', 'exec', namespace] if namespace else []
        text = command(*prefix, 'ethtool', '-K', interface, 'tx', 'off', 'tso', 'off', 'gso', 'off', 'gro', 'off')
        features = command(*prefix, 'ethtool', '-k', interface)
        for feature in ['tx-checksumming', 'tcp-segmentation-offload', 'generic-segmentation-offload', 'generic-receive-offload']:
            assert any(line.startswith(feature + ': off') for line in features.splitlines()), feature
        (path/(interface+'-features.log')).write_text(text+features)
        (path/(interface+'-channels.log')).write_text(command(*prefix, 'ethtool', '-l', interface))


def check_affinity(snapshot, allowed):
    for process in snapshot['processes'].values():
        for thread in process['threads']:
            cpus = set()
            for part in thread['cpus'].split(','):
                bounds = [int(x) for x in part.split('-')]
                cpus.update(range(bounds[0], bounds[-1] + 1))
            assert cpus and cpus <= set(allowed), thread


def execute(args, directory, target, concurrency, payload, cpus):
    directory.mkdir()
    origin = server = client = tracer = None
    record = {'target': target, 'concurrency': concurrency, 'payload': payload,
              'passed': False, 'started_unix': time.time(), 'errors': []}
    nodes = COMPARISON_NODES[args.comparison]
    proxy_cpus = cpus[:args.proxy_cpus]
    iterations = args.single_flow_iterations if concurrency == 1 and args.single_flow_iterations is not None else args.iterations
    record.update(proxy_cpus=proxy_cpus, iterations_per_flow=iterations)
    logs = []
    def spawn(argv, name, env=None):
        output = (directory/(name+'.log')).open('w'); logs.append(output)
        return subprocess.Popen([str(a) for a in argv], cwd=ROOT, env=env,
                                stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        compiler_guard()
        origin = spawn(['ip','netns','exec',NS,'taskset','-c',str(cpus[3]),args.loadgen,
                        'origin','10.203.0.2','19003',directory/'origin-ready.json',directory/'origin.json','valid'], 'origin')
        assert wait_file(directory/'origin-ready.json', origin, as_json=True)['port'] == 19003
        if target != 'direct':
            if target in nodes:
                wake, rx_stats, idle_yield, udp_payload = nodes[target]
                configure(directory/'home',proxy_cpus[0])
                env = dict(os.environ, ZELVIK_EDGE_HOME=str(directory/'home'),
                           ZELVIK_EDGE_XDP_PIN_DIR=str(Path('/sys/fs/bpf/edge-udp')/directory.name))
                argv = ['taskset','-c',','.join(map(str,proxy_cpus)),args.edge,'xdp','proxy-smoke',
                        '--wake',wake,'--rx-stats',rx_stats,'--idle-yield',idle_yield,
                        '--udp-payload',udp_payload,
                        '--remote','10.203.0.2','--duration-ms','60000',
                        '--ready-file',directory/'node-ready']
                server = spawn(argv, 'server', env)
                wait_file(directory/'node-ready', server)
            else:
                binary = args.nginx if target == 'nginx' else args.openresty
                config = nginx_config(directory,proxy_cpus)
                argv = ['taskset','-c',','.join(map(str,proxy_cpus)),binary,'-p',str(directory/'prefix')+'/',
                        '-c',config,'-g','daemon off;']
                server = spawn(argv, 'server')
                wait_file(directory/'nginx.pid', server)
                deadline = time.monotonic() + 5
                while len(descendants(server.pid)) < 1+args.proxy_cpus:
                    assert server.poll() is None and time.monotonic() < deadline
                    time.sleep(.01)
            record['server_command'] = list(map(str,argv))
        fixed_network(directory)
        target_ip, port = ('10.203.0.2',19003) if target == 'direct' else ('10.203.0.1',443)
        argv = ['ip','netns','exec',NS,'taskset','-c',str(cpus[2]),args.loadgen,'gated-client',target_ip,
                str(port),str(concurrency),str(iterations),str(args.warmup),str(payload),
                directory/'client.json',directory/'samples.json',directory/'warm-ready.json',
                directory/'measure-go',directory/'measure-done.json']
        record['client_command'] = list(map(str,argv))
        client = spawn(argv, 'client')
        wait_file(directory/'warm-ready.json',client,as_json=True)
        if args.syscall_profile and server is not None:
            tracer = SyscallProfile(descendants(server.pid), directory)
            record['trace_attached_before'] = tracer.attach()
        record['cpu_before'] = cpu()
        record['origin_before'] = process_snapshot(origin.pid)
        check_affinity(record['origin_before'],[cpus[3]])
        record['client_after_warmup'] = process_snapshot(client.pid)
        check_affinity(record['client_after_warmup'],[cpus[2]])
        if server:
            record['server_before'] = process_snapshot(server.pid)
            check_affinity(record['server_before'],proxy_cpus)
        (directory/'measure-go').write_text('go\n')
        wait_file(directory/'measure-done.json',client,as_json=True)
        if server:
            record['server_after'] = process_snapshot(server.pid)
            check_affinity(record['server_after'],proxy_cpus)
            assert record['server_before']['processes'].keys() == record['server_after']['processes'].keys(), 'server processes changed'
        record['origin_after'] = process_snapshot(origin.pid)
        record['cpu_after'] = cpu()
        if tracer is not None:
            tracer.check_after_measurement()
            record['syscall_profile'] = tracer.finish()
        assert client.wait(timeout=15) == 0
        client = None
        data = json.loads((directory/'client.json').read_text())
        samples = json.loads((directory/'samples.json').read_text())
        assert data['passed'] and len(samples) == iterations*concurrency
        assert data['sent'] == data['received'] == (iterations+args.warmup)*concurrency
        assert data['elapsed_ns'] == data['timed_end_ns'] - data['timed_start_ns']
        assert record['cpu_before']['monotonic_ns'] <= data['timed_start_ns'] <= data['timed_end_ns'] <= record['cpu_after']['monotonic_ns']
        if tracer is not None:
            trace = record['syscall_profile']
            assert trace['completed']
            assert trace['attached_before']['monotonic_ns'] <= record['cpu_before']['monotonic_ns']
            assert record['cpu_after']['monotonic_ns'] <= trace['attached_after']['monotonic_ns'] <= trace['stop_requested_monotonic_ns']
        ordered = sorted(samples)
        assert data['p99_rtt_ns'] == ordered[(len(samples)*99+99)//100-1]
        assert data['p50_rtt_ns'] == ordered[(len(samples)-1)//2]
        if server:
            record['idle_before'] = process_snapshot(server.pid)
            time.sleep(2.2)
            record['idle_after'] = process_snapshot(server.pid)
        stop_process(server); server = None
        stop_process(origin); origin = None
        origin_data = json.loads((directory/'origin.json').read_text())
        assert origin_data['passed'] and origin_data['received'] == origin_data['sent'] == data['sent']
        assert origin_data['distinct_peer_ports'] == concurrency, 'upstream socket reuse did not match'
        if target in nodes:
            report = next(o for o in json_reports((directory/'server.log').read_text()) if 'app' in o)
            assert report['xskWakeMode'] == nodes[target][0] and report['xskRxStatsMode'] == nodes[target][1]
            assert report['xskIdleYieldMode'] == nodes[target][2]
            assert report['xskUdpPayloadMode'] == nodes[target][3]
            assert report['proxyReady'] and report['proxyRedirectEnabled']
            assert report['redirect'] == 2*data['sent'], 'both traffic directions must use XSK'
            assert report['xskDrops'] == 0 and all(value == 0 for value in report['xskQueueStats'].values())
            record['node_report'] = report
        record['result'] = data
        record['origin'] = origin_data
        record['passed'] = True
    except Exception:
        record['errors'].append(traceback.format_exc())
    finally:
        if tracer is not None and not tracer.stopped:
            try:
                tracer.abort()
            except Exception:
                record['errors'].append(traceback.format_exc());record['passed'] = False
        for process in [client,server,origin]:
            try:
                stop_process(process)
            except Exception:
                record['errors'].append(traceback.format_exc());record['passed'] = False
        for stream in logs:
            stream.close()
        pin = Path('/sys/fs/bpf/edge-udp')/directory.name
        try:
            if pin.exists():
                import shutil
                shutil.rmtree(pin)
            record['pin_cleanup_ok'] = True
        except Exception:
            record['errors'].append(traceback.format_exc());record['passed'] = False
            record['pin_cleanup_ok'] = False
        record['finished_unix'] = time.time()
        write(directory/'record.json',record)
    return record


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ['edge','nginx','openresty','loadgen','output']:
        p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--iterations',type=int,default=10000)
    p.add_argument('--single-flow-iterations',type=int)
    p.add_argument('--comparison',choices=COMPARISON_NODES,default='wake')
    p.add_argument('--proxy-cpus',type=int,choices=[1,2],default=2)
    p.add_argument('--syscall-profile',action='store_true',help='counts-only tracer after warmup; all timing is diagnostic')
    p.add_argument('--warmup',type=int,default=1000)
    p.add_argument('--rounds',type=int,default=4)
    args=p.parse_args()
    assert os.geteuid()==0 and args.rounds>=4 and args.rounds%4==0 and 0<args.iterations<=125000 and 0<=args.warmup<=125000
    assert args.single_flow_iterations is None or 0<args.single_flow_iterations<=2000000
    targets=list(COMPARISON_NODES[args.comparison])+['nginx','openresty']
    for name in ['edge','nginx','openresty','loadgen']:
        setattr(args,name,getattr(args,name).resolve(strict=True))
    output=args.output.resolve();output.mkdir(parents=True,exist_ok=False)
    cpus=sorted(os.sched_getaffinity(0))[:4]
    assert len(cpus)==4,'four CPUs required: proxy1/2, client1, origin1'
    binaries={n:digest(getattr(args,n)) for n in ['edge','nginx','openresty','loadgen']}
    sources=[ROOT/'src/xdp/af_xdp/bridge.rs',ROOT/'src/xdp/af_xdp/readiness.rs',ROOT/'src/xdp/af_xdp/wake.rs',ROOT/'src/xdp/smoke.rs']
    sources.append(ROOT/'benches/support/udp-bench.c')
    sources += [ROOT/'src/xdp/af_xdp/cooperation.rs', ROOT/'src/xdp/af_xdp/cooperation_tests.rs']
    sources += [ROOT/'src/udp_proxy.rs', ROOT/'src/xdp/af_xdp/dial.rs']
    sources += [ROOT/'src/xdp/linux.rs',ROOT/'src/runtime_mode.rs',ROOT/'src/main.rs',ROOT/'scripts/xdp-netns-smoke.sh']
    sources+=sorted((ROOT/'scripts/perf/afxdp-node').glob('*'))
    hashes={str(f.relative_to(ROOT)):digest(f) for f in sources if f.is_file()}
    metadata={'source_commit':command('git','rev-parse','HEAD').strip(),
          'binaries':binaries,'sources':hashes,'cpus':cpus,'uname':platform.uname()._asdict(),
          'clock_ticks':os.sysconf('SC_CLK_TCK'),
          'page_size':os.sysconf('SC_PAGE_SIZE'),
          'scope':f'IPv4 UDP steady-flow reuse; single XSK queue, {args.proxy_cpus} proxy CPUs, separate client and remote origin CPUs; no HTTP/cache/hardware-zero-copy claim',
          'comparison':args.comparison,'targets':targets,'node_modes':COMPARISON_NODES[args.comparison],'proxy_cpus':cpus[:args.proxy_cpus],
          'syscall_profile':args.syscall_profile,
          'single_flow_iterations':args.single_flow_iterations,
          'iterations':args.iterations,'warmup':args.warmup,'rounds':args.rounds,
          'nginx_udp_policy':'proxy_requests 0; proxy_responses 2147483647; proxy_timeout 60s; actual peer ports verified'}
    if args.syscall_profile:
        metadata['scope'] = 'ptrace syscall diagnostics on IPv4 UDP reuse; timing/CPU perturbed, not native performance or capacity'
        metadata['strace_version'] = command('strace','--version').splitlines()[0]
    write(output/'metadata.json',metadata)
    records=[]
    for c in [1,16]:
        for payload in [64,1200]:
            records.append(execute(args,output/f'direct-c{c}-b{payload}','direct',c,payload,cpus))
            for repeat in range(args.rounds):
                # Balanced Williams design: each target occupies every position.
                order=[targets[i] for i in [[0,1,3,2],[1,2,0,3],[2,3,1,0],[3,0,2,1]][repeat%4]]
                for target in order:
                    label=f'r{repeat+1}-c{c}-b{payload}-{target}'
                    print('running',label,flush=True)
                    record=execute(args,output/label,target,c,payload,cpus);record['round']=repeat+1
                    write(output/label/'record.json',record)
                    records.append(record)
                    write(output/'runs.json',records)
    assert binaries=={n:digest(getattr(args,n)) for n in binaries},'binaries changed'
    assert hashes=={str(f.relative_to(ROOT)):digest(f) for f in sources if f.is_file()},'sources changed'
    summary=summarize(records,metadata)
    if args.syscall_profile:
        summary['interpretation'] = 'Instrumented ptrace diagnostics only; RPS/RTT/CPU are perturbed, never native capacity. All attempts and outliers retained. Syscall wall time sums observed waits across threads, not CPU consumption. Trace excludes warmup and the 2.2s idle observation; includes snapshot boundary overhead.'
    summary.update(binaries_unchanged=True,sources_unchanged=True)
    write(output/'summary.json',summary)
    raise SystemExit(0 if summary['passed'] else 1)


if __name__=='__main__':
    main()
