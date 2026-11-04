#!/usr/bin/env python3
"""Independent production-startup proxy comparison: protocol, cache, connection.

Run one target per invocation. Every invocation owns its origin and Docker
network. Measurements never overlap a Rust compiler. Test secrets stay local.
"""
import argparse
from collections import Counter
import gzip
import hashlib
import json
import os
from pathlib import Path
import random
import resource
import shutil
import subprocess
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
DOCKER = ['docker', '--host=unix:///var/run/docker.sock']
ENV = {k: v for k, v in os.environ.items() if k not in {
    'DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_TLS', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH'}}
IMAGES = {'nginx': 'nginx:1.28.0',
          'openresty': 'openresty/openresty:1.27.1.2-0-bookworm-fat',
          'edge': 'edge-quick-runtime:20261101'}
VERSIONS = {'h1': '1.1', 'h1-tls': '1.1', 'h2': '2', 'h3': '3'}
FLAGS = {'h1': '--http1.1', 'h1-tls': '--http1.1',
         'h2': '--http2-prior-knowledge', 'h3': '--http3-only'}


def docker(*args):
    return subprocess.check_output([*DOCKER, *map(str, args)], env=ENV,
                                   stderr=subprocess.STDOUT, text=True).strip()


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write(path, data):
    path.write_text(json.dumps(data, indent=2) + '\n')


def refuse_compilation():
    for p in Path('/proc').glob('[0-9]*/comm'):
        try:
            if p.read_text().strip() in {'rustc', 'mbx-rustc'}:
                if not any(s.startswith('State:') and s.split()[1] == 'Z'
                           for s in p.with_name('status').read_text().splitlines()):
                    raise RuntimeError('Benchmark cannot overlap Rust compilation')
        except FileNotFoundError:
            continue


def host_cpu():
    return {line.split()[0]: list(map(int, line.split()[1:]))
            for line in Path('/proc/stat').read_text().splitlines()
            if line.startswith('cpu')}


def cgroup(name):
    text = docker('exec', name, 'sh', '-c',
                  'for f in cpu.stat memory.current memory.peak memory.events; do '
                  'echo @$f; cat /sys/fs/cgroup/$f || exit; done')
    result = {}; current = None
    for line in text.splitlines():
        if line.startswith('@'):
            current = line[1:]; result[current] = {}
        else:
            fields = line.split()
            result[current][fields[0] if len(fields) == 2 else 'bytes'] = int(fields[-1])
    return result


def filesystems(name):
    lines=docker('exec',name,'df','-Pk','/node','/run/cache').splitlines()[1:]
    result={}
    for line in lines:
        fields=line.split()
        result[fields[-1]]={'size_kib':int(fields[1]),'used_kib':int(fields[2]),
                            'available_kib':int(fields[3]),'used_percent':int(fields[4].rstrip('%'))}
    return result


def raw_curl(url, extra=()):
    return subprocess.check_output(['curl', '--noproxy', '*', '--silent', '--show-error',
                                    '--max-time', '20', *extra, url])


def origin_count(port):
    return int(raw_curl(f'http://127.0.0.1:{port}/_stats').decode().splitlines()[2].split()[2])


def process_cpu_snapshot(processes):
    """Optional Linux host-PID observation, outside the measured client call."""
    rows = []
    for line in processes.splitlines():
        fields = line.split()
        if not fields or not fields[0].isdigit():
            continue
        pid = int(fields[0])
        raw = Path(f'/proc/{pid}/stat').read_text()
        end = raw.rindex(')')
        values = raw[end + 2:].split()
        rows.append({'pid': pid, 'comm': raw[raw.index('(') + 1:end],
                     'ppid': int(values[1]), 'start_ticks': int(values[19]),
                     'user_ticks': int(values[11]), 'system_ticks': int(values[12])})
    if not rows:
        raise RuntimeError('process CPU observation requires Linux host PID namespace')
    return {'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'), 'processes': rows}


def checked_curl_rows(path, protocol, size, expected, fresh=True):
    rows = []
    for line in path.read_text().splitlines():
        fields=line.split('\t')
        code, version, connects, received, total, first, exitcode = fields[:7]
        row = {'code': int(code), 'version': version, 'connects': int(connects),
               'bytes': int(received), 'seconds': float(total),
               'ttfb': float(first), 'exitcode': int(exitcode)}
        rows.append(row)
        if len(fields)>7: row['connection_id']=int(fields[7])
    if expected is not None and len(rows) != expected:
        raise RuntimeError(f'curl returned {len(rows)} records, expected {expected}')
    failures = [row for row in rows if row['code'] != 200 or row['exitcode'] != 0
                or row['version'] != VERSIONS[protocol] or row['bytes'] != size
                or fresh and row['connects'] != 1]
    return rows, failures


def cache_contract(state, origin_requests, successful, has_errors):
    if state == 'hit' and origin_requests != 0:
        raise RuntimeError(f'warmed cache reached origin {origin_requests} times')
    if state in {'off', 'miss'} and not has_errors and origin_requests != successful:
        raise RuntimeError(f'{state}: origin={origin_requests}, HTTP200={successful}')


def probe(protocol, url, ca, payload, stem):
    body = stem.with_suffix('.body'); headers = stem.with_suffix('.headers')
    command = ['curl', '--ipv4', '--noproxy', '*', '--silent', '--show-error', '--max-time', '20',
               '--user-agent', 'edge-protocol-state-matrix/1',
               FLAGS[protocol], '--cacert', str(ca), '--output', str(body),
               '--dump-header', str(headers), '--write-out',
               '%{http_version}\t%{ssl_verify_result}\t%{http_code}', url]
    p = subprocess.run(command, capture_output=True, text=True, timeout=25)
    if p.returncode or p.stdout != f'{VERSIONS[protocol]}\t0\t200':
        raise RuntimeError(f'integrity failed: {p.stdout} {p.stderr[-1200:]}')
    if body.read_bytes() != payload:
        raise RuntimeError('integrity response body mismatch')
    cache_headers=[line.split(':',1)[1].strip() for line in headers.read_text().splitlines()
                   if line.lower().startswith('x-cache:')]
    return {'protocol': VERSIONS[protocol], 'tls_verified': True,
            'cache_status':cache_headers[-1] if cache_headers else None,
            'sha256': sha(body), 'headers_file': headers.name}


def run_client(args, protocol, state, connection, url, ca, stem):
    limit = args.fresh_requests if connection == 'fresh' else args.cold_requests if state == 'miss' else None
    if args.concurrency % 2 or limit and limit % 2:
        raise ValueError('request count and concurrency must be even')
    if state == 'miss':
        url = url.rsplit('/', 1)[0]
    processes = []; paths = []; commands = []
    for shard, cpu in enumerate(args.cpu_assignment['client']):
        path = stem.with_suffix(f'.{shard}.client.tsv'); paths.append(path)
        command = ['taskset', '-c', str(cpu), str(args.client), '--protocol', protocol,
                   '--url', url, '--cacert', str(ca), '--payload', str(stem.parent/'payload'),
                   '--connections', str(args.concurrency // 2), '--seconds', str(args.seconds)]
        if limit: command += ['--requests', str(limit // 2)]
        if state == 'miss': command += ['--unique']
        if connection == 'fresh': command += ['--fresh']
        if connection == 'multiplex': command += ['--multiplex']
        if getattr(args, 'fixture_ca_only', False): command += ['--fixture-ca-only']
        if getattr(args, 'client_curves', None): command += ['--curves', args.client_curves]
        commands.append(command)
        with path.open('x') as out, stem.with_suffix(f'.{shard}.stderr').open('x') as err:
            processes.append(subprocess.Popen(command, stdout=out, stderr=err))
    write(stem.with_suffix('.command.json'), commands)
    try:
        returncodes = [p.wait(timeout=max(90, limit / 100) if limit else args.seconds + 25) for p in processes]
    finally:
        for p in processes:
            if p.poll() is None:
                p.kill(); p.wait()
    if any(returncodes):
        raise RuntimeError(f'native libcurl client failed: {returncodes}')
    return {'paths':paths, 'limit':limit}


def parse_client(args, protocol, connection, raw, elapsed):
    rows = []; failures = []
    for path in raw['paths']:
        records, bad = checked_curl_rows(path, protocol, args.payload_size,
                                        raw['limit'] // 2 if raw['limit'] else None,
                                        fresh=connection == 'fresh')
        rows.extend(records); failures.extend(bad)
        # Preserve every transfer without growing unbounded uncompressed logs.
        with path.open('rb') as source, gzip.open(str(path)+'.gz', 'wb') as dest:
            shutil.copyfileobj(source, dest)
        path.unlink()
    if not rows:
        raise RuntimeError('native client returned no transfer records')
    latency = sorted(row['seconds'] for row in rows if row['code'] == 200 and row['exitcode'] == 0)
    return {'client':'native-libcurl-multi', 'request_limit':raw['limit'],
            'successful':len(rows)-len(failures), 'failed':len(failures),
            'error_distribution':dict(Counter(f"curl_{r['exitcode']}/http_{r['code']}/v_{r['version']}" for r in failures)),
            'first_failures':failures[:8], 'new_connections':sum(row['connects'] for row in rows),
            'good_rps':(len(rows)-len(failures))/elapsed,
            'p99_ms':latency[min(len(latency)-1,int(len(latency)*.99))]*1000 if latency else None}


def ports(name):
    p = json.loads(docker('inspect', name))[0]['NetworkSettings']['Ports']
    return {k: int(v[0]['HostPort']) for k, v in p.items() if v}


def prepare(args, out, tls):
    unit = b'edge-protocol-state-matrix\n'
    payload = (unit * (args.payload_size // len(unit) + 1))[:args.payload_size]
    (out/'payload').write_bytes(payload); (out/'payload').chmod(0o644)
    (out/'origin.conf').write_text('''worker_processes 1;
events { worker_connections 8192; }
http { access_log off; sendfile on; keepalive_requests 1000000;
server { listen 8081; location = /_stats { stub_status; }
location / { root /test; try_files /payload =404; add_header Cache-Control "public, max-age=3600" always; } } }
''')
    quic = 'listen 19443 quic reuseport;' if getattr(args, 'supports_h3', True) else ''
    (out/'proxy.conf').write_text('worker_processes '+str(len(args.cpu_assignment['proxy']))+''';
worker_rlimit_nofile 65536;
events { worker_connections 16384; }
http { access_log off; error_log /dev/stderr warn; sendfile on; gzip off;
keepalive_timeout 60; keepalive_requests 1000000;
proxy_cache_path /run/cache levels=1:2 keys_zone=matrix:32m max_size=256m inactive=1h;
map $upstream_cache_status $cache_state { "" BYPASS; default $upstream_cache_status; }
upstream source { server origin:8081; keepalive 128; }
server { listen 19080; listen 19443 ssl;
''' + quic + '''
http2 on; ssl_certificate /tls/server.pem; ssl_certificate_key /tls/server.key;
ssl_protocols TLSv1.2 TLSv1.3; ssl_session_cache off; ssl_session_tickets off;
proxy_http_version 1.1; proxy_set_header Connection ""; proxy_set_header Host localhost;
proxy_buffering on; proxy_cache_key "$scheme$host$request_uri";
add_header X-Cache $cache_state always;
location /off/ { proxy_cache off; proxy_pass http://source; }
location /hit/ { proxy_cache matrix; proxy_cache_valid 200 1h; proxy_pass http://source; }
location /miss/ { proxy_cache matrix; proxy_cache_valid 200 1h; proxy_pass http://source; }
} }
''')
    server = {'id':1, 'userId':1, 'isOn':True, 'serverNames':[{'name':'localhost'}],
              'http':{'isOn':True,'listen':[{'host':'0.0.0.0','portRange':'19080','protocol':'http'}]},
              'https':{'isOn':True,'http3Enabled':True,
                       'listen':[{'host':'0.0.0.0','portRange':'19443','protocol':'https'}],
                       'sslPolicy':{'id':1,'isOn':True,'http2Enabled':True,'minVersion':'TLSv1.2',
                                    'certs':[{'id':1,'isOn':True,'isDefault':True,'dnsNames':['localhost'],
                                              'certData':(tls/'server.pem').read_text(),
                                              'keyData':(tls/'server.key').read_text()}]}},
              'reverseProxy':{'isOn':True,'primaryOrigins':[{'id':1,'isOn':True,'addr':'http://origin:8081','weight':100,
                                                           'followHost':False,'followPort':False,'http2Enabled':False}],
                              'scheduling':{'code':'polling','options':{}}},
              'web':{'isOn':True,'cache':{'isOn':True,'cacheRefs':[{'isOn':True,
                        'life':{'count':3600,'unit':'s'},'methods':['GET'],'status':[200],
                        'maxSize':{'count':8,'unit':'m'},
                        'onlyURLPatterns':[{'type':'prefix','pattern':'/hit/'},{'type':'prefix','pattern':'/miss/'}]}]}}}
    write(out/'overrides.json', {'servers':[server],'metricItems':[],
         'http3Policies':{'1':{'isOn':True,'port':19443,
                             'addressValidation':getattr(args, 'address_validation', 'adaptive')}},
         'globalServerConfig':{'httpAll':{'allowLocalOrigins':True,'supportsLowVersionHTTP':True},
                               'httpAccessLog':{'isOn':False}}})
    (out/'overrides.json').chmod(0o600)
    return payload


def cpu_assignment(allowed):
    if len(allowed) < 4:
        raise RuntimeError('requires at least four distinct available CPU identities')
    proxy_count = 2 if len(allowed) >= 5 else 1
    return {'proxy':allowed[:proxy_count], 'client':allowed[proxy_count:proxy_count+2],
            'origin':[allowed[proxy_count+2]]}


def wait_origin_ready(origin, url, ca, payload, stem):
    """Validate origin startup before the single, unretried capacity sample."""
    deadline = time.monotonic() + 120
    attempts = []
    while True:
        try:
            observation = probe('h1', url, ca, payload, stem)
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            attempts.append({'status': 'not-ready', 'error': str(error)})
            write(stem.with_suffix('.json'), {'attempts': attempts})
            running = json.loads(docker('inspect', origin))[0]['State']['Status'] == 'running'
            if time.monotonic() >= deadline or not running:
                raise RuntimeError('origin readiness failed; retained startup probes') from error
            time.sleep(.5)
        else:
            attempts.append({'status': 'ready', 'observation': observation})
            write(stem.with_suffix('.json'), {'attempts': attempts})
            return {'attempts': len(attempts), 'observation': observation}


def run(args):
    refuse_compilation()
    args.cpu_assignment = cpu_assignment(sorted(os.sched_getaffinity(0)))
    running = docker('ps', '--format', '{{.Names}}').splitlines()
    harness = os.environ.get('EDGE_MATRIX_HARNESS_NAME')
    if any(name != harness for name in running):
        raise RuntimeError('independent benchmark requires no other running containers')
    if getattr(args, 'edge_image', None): IMAGES['edge'] = args.edge_image
    if args.target == 'pingora':
        if args.states != ['off'] or not args.protocols or 'h3' in args.protocols:
            raise ValueError('bare Pingora diagnostic supports explicit H1/H1-TLS/H2 OFF only')
        IMAGES['pingora'] = IMAGES['edge']
    args.supports_h3 = True
    capability = 'Edge Quinn H3 enabled in this build'
    if args.target == 'pingora':
        capability = docker('run', '--rm', '--mount',
            f'type=bind,src={args.bare_pingora_binary.resolve()},dst=/tools/bare-pingora,readonly',
            '--entrypoint', '/tools/bare-pingora', IMAGES['pingora'], '--version')
        args.supports_h3 = False
    elif args.target != 'edge':
        executable = 'openresty' if args.target == 'openresty' else 'nginx'
        capability = docker('run', '--rm', '--entrypoint', executable, IMAGES[args.target], '-V')
        args.supports_h3 = '--with-http_v3_module' in capability
    out = args.output.resolve(); out.mkdir(parents=True, exist_ok=False); out.chmod(0o755)
    tls = args.tls.resolve(); payload = prepare(args, out, tls)
    client_source = ROOT / 'benches/support/protocol_state_client.c'
    source_revision = getattr(args, 'client_source_revision', None)
    source_sha = sha(client_source)
    if source_revision:
        if len(source_revision) != 40 or any(c not in '0123456789abcdef' for c in source_revision):
            raise ValueError('client source revision must be a complete immutable commit SHA')
        source_sha = hashlib.sha256(subprocess.check_output(
            ['git', 'show', source_revision + ':benches/support/protocol_state_client.c'], cwd=ROOT)).hexdigest()
    manifest = {'target':args.target,'started_at':time.time(),'cases':[], 'status':'running',
                'args':{k:str(v) if isinstance(v,Path) else v for k,v in vars(args).items()},
                'source_revision':subprocess.check_output(['git','rev-parse','HEAD'],cwd=ROOT,text=True).strip(),
                'driver_sha256':sha(__file__),'client_sha256':sha(args.client),
                'client_source_sha256':source_sha, 'client_source_revision':source_revision,
                'harness_client_source_sha256':sha(client_source),'payload_sha256':sha(out/'payload'),
                'cpu_assignment':args.cpu_assignment, 'protocol_capability':capability,
                'inter_case_quiet_seconds':0.5,
                'case_isolation':'fresh process and tmpfs for EVERY protocol/cache/connection/repeat case',
                'parent_limits':{f:Path('/sys/fs/cgroup',f).read_text().strip() for f in ['cpu.max','memory.max']},
                'images':{t:json.loads(docker('image','inspect',i))[0]['Id'] for t,i in IMAGES.items()},
                'scope':'production Edge startup + authenticated test controller in same proxy cgroup' if args.target=='edge' else 'packaged server proxy',
                'client_versions':{'curl':subprocess.check_output(['curl','--version'],text=True),
                                   'native_libcurl':subprocess.check_output([str(args.client),'--version'],text=True)}}
    if args.target == 'edge':
        manifest['binaries']={str(p):sha(p) for p in [args.edge_binary,args.controller]}
    elif args.target == 'pingora':
        manifest['binaries'] = {str(args.bare_pingora_binary): sha(args.bare_pingora_binary)}
        manifest['scope'] = 'OFF-only framework diagnostic; no Edge business policies or telemetry'
    def save(): write(out/'manifest.json',manifest)
    save(); prefix='edge-state-'+uuid.uuid4().hex[:10]; net=prefix+'-net'; names=[]; made_net=False
    try:
        docker('network','create',net); made_net=True
        origin=prefix+'-origin'; names.append(origin)
        docker('run','-d','--name',origin,'--network',net,'--network-alias','origin',
               '--cpuset-cpus',str(args.cpu_assignment['origin'][0]),'--cpus','1','--memory','256m','--memory-swap','256m',
               '--mount',f'type=bind,src={out},dst=/test,readonly',
               '--mount',f'type=bind,src={out}/origin.conf,dst=/etc/nginx/nginx.conf,readonly',
               '-p','127.0.0.1::8081','nginx:1.28.0')
        origin_port=ports(origin)['8081/tcp']
        manifest['origin_readiness'] = wait_origin_ready(
            origin, f'http://localhost:{origin_port}/off/payload', tls/'ca.pem',
            payload, out/'origin-readiness')
        save()
        origin_before=cgroup(origin); usage_before=resource.getrusage(resource.RUSAGE_CHILDREN)
        started=time.monotonic()
        raw=run_client(args,'h1','off','reuse',f'http://localhost:{origin_port}/off/payload',tls/'ca.pem',out/'origin-reference')
        elapsed=time.monotonic()-started; usage_after=resource.getrusage(resource.RUSAGE_CHILDREN)
        origin_after=cgroup(origin)
        reference=parse_client(args,'h1','reuse',raw,elapsed)
        reference.update(elapsed=elapsed,
                         origin_cpu_fraction=(origin_after['cpu.stat']['usage_usec']-origin_before['cpu.stat']['usage_usec'])/(elapsed*1e6),
                         client_cpu_fraction=(usage_after.ru_utime+usage_after.ru_stime-usage_before.ru_utime-usage_before.ru_stime)/(2*elapsed))
        manifest['origin_reference']=reference; save()
        if reference['failed']:
            raise RuntimeError('direct origin capacity reference has transfer errors')
        target=prefix+'-'+args.target; names.append(target)
        command=['run','-d','--name',target,'--network',net,'--cpuset-cpus',','.join(map(str,args.cpu_assignment['proxy'])),'--cpus',str(len(args.cpu_assignment['proxy'])),
                 '--memory','2g','--memory-swap','2g','--ulimit','nofile=65536:65536',
                 '--tmpfs','/run/cache:rw,size=512m,mode=1777', '--tmpfs','/node:rw,size=512m,mode=1777',
                 '--mount',f'type=bind,src={out},dst=/test',
                 '--mount',f'type=bind,src={tls},dst=/tls,readonly',
                 '-p','127.0.0.1::19080','-p','127.0.0.1::19443/tcp','-p','127.0.0.1::19443/udp']
        if args.target=='edge':
            command += ['--mount',f'type=bind,src={args.edge_binary.resolve()},dst=/tools/Zelvik-Edge,readonly',
                        '--mount',f'type=bind,src={args.controller.resolve()},dst=/tools/controller,readonly',
                        '-e','RUST_LOG=warn',
                        '-e',f"ZELVIK_EDGE_H3_BACKEND={getattr(args, 'edge_backend', 'quinn-shared')}",
                        '-e',f"ZELVIK_EDGE_H3_TRANSPORT_PROFILE={getattr(args, 'transport_profile', 'production')}",
                        '-e',f"ZELVIK_EDGE_QUINN_NATIVE_GSO={getattr(args, 'quinn_native_gso', 'auto')}",
                        '-e',f"ZELVIK_EDGE_QUIC_RX_BATCH={getattr(args, 'quic_rx_batch', '1')}",'--entrypoint','/tools/controller',IMAGES['edge'],
                        '--home','/node','--spawn-edge','--edge-binary','/tools/Zelvik-Edge',
                        '--origin','http://origin:8081','--tls-port','19443','--tls-host','localhost',
                        '--ca-file','/tls/ca.pem','--cert-file','/tls/server.pem','--key-file','/tls/server.key',
                        '--snapshot-overrides','/test/overrides.json', '--edge-log','/test/edge.log',
                        '--status-file','/test/controller-status.json',
                        '--ready-file','/test/edge-ready.json','--telemetry-mode','success',
                        '--telemetry-store','/node/accepted.bin']
        elif args.target == 'pingora':
            command += ['--mount', f'type=bind,src={args.bare_pingora_binary.resolve()},dst=/tools/bare-pingora,readonly',
                        '--entrypoint', '/tools/bare-pingora', IMAGES['pingora'],
                        '--origin', 'origin:8081', '--cert', '/tls/server.pem', '--key', '/tls/server.key',
                        '--threads', str(len(args.cpu_assignment['proxy'])), '--pool', '256']
        else:
            executable='openresty' if args.target=='openresty' else 'nginx'
            command+=['--entrypoint',executable,IMAGES[args.target],'-c','/test/proxy.conf','-g','daemon off;']
        docker(*command)
        def wait_ready():
            exposed=ports(target)
            endpoints={'h1':f'http://localhost:{exposed["19080/tcp"]}',
                       'h1-tls':f'https://localhost:{exposed["19443/tcp"]}',
                       'h2':f'https://localhost:{exposed["19443/tcp"]}',
                       'h3':f'https://localhost:{exposed["19443/udp"]}'}
            manifest['ports']=exposed; save()
            deadline=time.monotonic()+120
            while True:
                try:
                    if args.target=='edge':
                        ready=json.loads((out/'edge-ready.json').read_text())
                        if ready.get('status')!='edge-ready':
                            raise RuntimeError('production Edge has not reported NodeControl health')
                        manifest['readiness']={k:ready.get(k) for k in ['status','edge_pid','counters']}
                    probe('h1',endpoints['h1']+'/off/payload',tls/'ca.pem',payload,out/'readiness')
                    return endpoints
                except (OSError,RuntimeError,subprocess.SubprocessError):
                    if time.monotonic()>deadline or json.loads(docker('inspect',target))[0]['State']['Status']!='running':
                        raise RuntimeError('proxy readiness failed: '+docker('logs',target)[-3000:])
                    time.sleep(.5)
        endpoints=wait_ready()
        manifest['inspect']={n:json.loads(docker('inspect',n))[0]['HostConfig'] for n in names}
        executable = {'edge': '/tools/Zelvik-Edge', 'pingora': '/tools/bare-pingora',
                      'openresty': 'openresty', 'nginx': 'nginx'}[args.target]
        manifest['server_version'] = docker('exec', target, executable,
            '--version' if args.target in {'edge', 'pingora'} else '-V')
        rng=random.Random(args.seed)
        for repeat in range(args.repeats):
            cases=[(p,s,c) for p in VERSIONS for s in args.states for c in args.connections]
            if args.multiplex:
                cases += [(p,s,'multiplex') for p in ['h2','h3'] for s in args.states]
            if args.protocols: cases=[case for case in cases if case[0] in args.protocols]
            rng.shuffle(cases)
            for protocol,state,connection in cases:
                if protocol == 'h3' and not args.supports_h3:
                    manifest['cases'].append({'protocol':protocol,'cache':state,'connection':connection,
                        'repeat':repeat,'stem':f'unsupported-{len(manifest["cases"]):04d}',
                        'fresh_instance':False,'status':'not-supported',
                        'reason':'actual server -V lacks --with-http_v3_module'})
                    save()
                    continue
                refuse_compilation()
                if manifest['cases']:
                    docker('stop','--time','0',target)
                    if args.target=='edge':
                        (out/'edge-ready.json').unlink(missing_ok=True)
                        (out/'controller-status.json').unlink(missing_ok=True)
                        if (out/'edge.log').exists():
                            (out/'edge.log').rename(out/f"edge-boot-{len(manifest['cases'])-1:04d}.log")
                    docker('start',target)
                    endpoints=wait_ready()
                stem=out/f'{len(manifest["cases"]):04d}-{protocol}-{state}-{connection}'
                row={'protocol':protocol,'cache':state,'connection':connection,'repeat':repeat,'stem':stem.name,
                     'ports':dict(manifest['ports']),'fresh_instance':True}
                manifest['cases'].append(row); save()
                try:
                    url=endpoints[protocol]+f'/{state}/payload'
                    if state=='miss': url=endpoints[protocol]+f'/miss/{uuid.uuid4().hex}/payload'
                    row['integrity_before']=probe(protocol,url,tls/'ca.pem',payload,Path(str(stem)+'-before'))
                    cache_status=row['integrity_before']['cache_status']
                    if state=='miss' and cache_status!='MISS':
                        raise RuntimeError(f'requested cold miss but actual X-Cache is {cache_status}')
                    if state=='hit' and cache_status not in {'MISS','HIT'}:
                        raise RuntimeError(f'requested cache hit but actual X-Cache is {cache_status}')
                    if state=='hit':
                        # Wait for Edge's real metadata writer and cache publish.
                        for _ in range(60):
                            start=origin_count(origin_port)
                            warmed=probe(protocol,url,tls/'ca.pem',payload,Path(str(stem)+'-warm'))
                            if origin_count(origin_port)-start-1==0 and warmed['cache_status']=='HIT': break
                            time.sleep(.1)
                        else: raise RuntimeError('cache never became an origin-free hit')
                    if getattr(args, 'idle_reference_seconds', 0) > 0:
                        idle_before=cgroup(target)
                        idle_started=time.monotonic()
                        time.sleep(args.idle_reference_seconds)
                        idle_elapsed=time.monotonic()-idle_started
                        idle_after=cgroup(target)
                        idle_cpu=idle_after['cpu.stat']['usage_usec']-idle_before['cpu.stat']['usage_usec']
                        row['idle_reference']={'seconds':idle_elapsed,'cpu_usec':idle_cpu,
                                               'cpu_cores':idle_cpu/(idle_elapsed*1e6)}
                        refuse_compilation()
                    disk_before=filesystems(target)
                    processes_before=docker('top',target,'-eo','pid,comm,rss,nlwp')
                    before=cgroup(target); origin_resources_before=cgroup(origin)
                    origin_before=origin_count(origin_port); cpu_before=host_cpu()
                    process_before = process_cpu_snapshot(processes_before) if getattr(args, 'capture_proxy_process_cpu', False) else None
                    usage_before=resource.getrusage(resource.RUSAGE_CHILDREN); started=time.monotonic()
                    raw=run_client(args,protocol,state,connection,url,tls/'ca.pem',stem)
                    elapsed=time.monotonic()-started; usage_after=resource.getrusage(resource.RUSAGE_CHILDREN)
                    process_after = process_cpu_snapshot(processes_before) if process_before is not None else None
                    cpu_after=host_cpu(); origin_after=origin_count(origin_port); after=cgroup(target)
                    origin_resources_after=cgroup(origin)
                    disk_after=filesystems(target)
                    result=parse_client(args,protocol,connection,raw,elapsed); row['native']=result
                    row.update(elapsed=elapsed,origin_requests=origin_after-origin_before-1,
                               origin_counter_before=origin_before,origin_counter_after=origin_after,
                               proxy_processes_before=processes_before,
                               proxy_processes_after=docker('top',target,'-eo','pid,comm,rss,nlwp'),
                               proxy_before=before,proxy_after=after,host_cpu_before=cpu_before,host_cpu_after=cpu_after,
                               origin_before=origin_resources_before,origin_after=origin_resources_after,
                               filesystem_before=disk_before,filesystem_after=disk_after,
                               client_cpu_seconds=usage_after.ru_utime+usage_after.ru_stime-usage_before.ru_utime-usage_before.ru_stime)
                    if process_before is not None:
                        row.update(proxy_process_cpu_before=process_before, proxy_process_cpu_after=process_after)
                    cpu_usec=after['cpu.stat']['usage_usec']-before['cpu.stat']['usage_usec']
                    row['proxy_cpu_usec_per_200']=cpu_usec/result['successful'] if result['successful'] else None
                    cache_contract(state,row['origin_requests'],result['successful'],bool(result['failed']))
                    row['integrity_after']=probe(protocol,url,tls/'ca.pem',payload,Path(str(stem)+'-after'))
                    if args.target=='edge':
                        log=(out/'edge.log').read_text(errors='replace')
                        if 'No space left on device' in log or 'fatal invariant failure' in log:
                            raise RuntimeError('fixture storage exhausted or production invariant failure; capacity not accepted')
                    row['status']='passed' if not result['failed'] else 'traffic-errors'
                except (OSError,ValueError,RuntimeError,subprocess.SubprocessError) as e:
                    row.update(status='failed',error=str(e))
                save()
                print(json.dumps({'target':args.target,**{k:row[k] for k in ['protocol','cache','connection','repeat','status']},
                                  'rps':row.get('native',{}).get('good_rps'),'error':row.get('error')}),flush=True)
                time.sleep(.5)
        manifest['processes']=docker('top',target,'-eo','pid,comm,rss,nlwp')
        if args.target=='edge':
            manifest['edge_final_status']=json.loads((out/'controller-status.json').read_text())
        manifest['status']='completed'
    except BaseException as e:
        manifest.update(status='failed',error=str(e)); raise
    finally:
        for name in reversed(names):
            try:
                (out/(name+'.log')).write_text(docker('logs',name))
                docker('rm','-fv',name)
            except subprocess.CalledProcessError as e:
                manifest.setdefault('cleanup_errors',[]).append(str(e))
        if made_net:
            try: docker('network','rm',net)
            except subprocess.CalledProcessError as e: manifest.setdefault('cleanup_errors',[]).append(str(e))
        manifest['ended_at']=time.time(); save()
    return int(any(c['status'] not in {'passed','not-supported'} for c in manifest['cases']))


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--target',choices=[*IMAGES, 'pingora'],required=True)
    p.add_argument('--output',type=Path,required=True)
    p.add_argument('--tls',type=Path,required=True)
    p.add_argument('--client',type=Path,required=True)
    p.add_argument('--client-source-revision',help='immutable source commit for a retained client binary')
    p.add_argument('--edge-binary',type=Path,default=Path('/workspace/edge/target/release/Zelvik-Edge'))
    p.add_argument('--controller',type=Path,default=Path('/workspace/edge/target/release/examples/issue34-controller'))
    p.add_argument('--bare-pingora-binary',type=Path,default=Path('/workspace/edge/.local/bare-pingora-ci/bare-pingora-proxy'))
    p.add_argument('--capture-proxy-process-cpu',action='store_true')
    p.add_argument('--edge-image')
    p.add_argument('--quic-rx-batch', choices=['1','32'], default='1')
    p.add_argument('--edge-backend', choices=['quinn-shared','quinn-native','tokio-quiche-native'], default='quinn-shared')
    p.add_argument('--transport-profile', choices=['production','matched'], default='production')
    p.add_argument('--quinn-native-gso', choices=['auto','off'], default='auto')
    p.add_argument('--address-validation', choices=['adaptive','always','off'], default='adaptive')
    p.add_argument('--seconds',type=int,default=8)
    p.add_argument('--repeats',type=int,default=3)
    p.add_argument('--payload-size',type=int,default=1024)
    p.add_argument('--concurrency',type=int,default=32)
    p.add_argument('--cold-requests',type=int,default=4096)
    p.add_argument('--fresh-requests',type=int,default=8192)
    p.add_argument('--protocols',nargs='+',choices=list(VERSIONS))
    p.add_argument('--states',nargs='+',choices=['off','hit','miss'],default=['off','hit','miss'])
    p.add_argument('--connections',nargs='+',choices=['reuse','fresh'],default=['reuse','fresh'])
    p.add_argument('--multiplex',action='store_true',help='also test H2/H3 with 2 connections x 16 streams')
    p.add_argument('--seed',type=int,default=20261102)
    p.add_argument('--idle-reference-seconds',type=float,default=0,
                   help='observe background CPU before the timed load; never subtract it automatically')
    raise SystemExit(run(p.parse_args()))
