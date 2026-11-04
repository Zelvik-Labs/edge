#!/usr/bin/env python3
"""Replay original benchmark artifacts without recompilation or deployment."""
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import statistics
import subprocess
import tempfile
import traceback
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen
import zipfile

REPO = 'Zelvik-Labs/edge'
ROOT = Path.cwd()
OUT = ROOT / '.local/evidence-review'
API = 'https://api.github.com/repos/' + REPO
SPECS = [
    ('syscalls-fixed', 41806190842, 'fdfdcb58fceed9ce6e829e9ddefe6e88fb5f61f8', 'framework-syscalls', 'verified-syscalls.json'),
    ('framework-source', 41807179554, '89bc26be5463559a14289ec24b75ebcf46e63cd2', 'framework-profile', 'verified-profiles.json'),
    ('cross-source', 41807179846, '89bc26be5463559a14289ec24b75ebcf46e63cd2', 'cross-product-profile', 'verified-profiles.json'),
    ('futex-startup', 41808935453, '922bd2e5fb65d571627b3d1ad64dda83536d0259', 'futex-sampled', 'verified-futex-sampled.json'),
    ('futex-progress', 41812436186, 'd3af75a7c0dcfbc86f2a88c14719e2068bb8bf8b', 'futex-sampled', 'verified-futex-sampled.json'),
    ('hyper', 41811512638, 'f9a174ce265110613471ba3b2666d738fea25c58', 'bare-hyper', 'verified-summary.json'),
    ('buffers', 41811855868, '5d5be77f6f90852912c504c28c2660819675c27d', 'pingora-buffers', 'verified-summary.json'),
]


def sha(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            value.update(block)
    return value.hexdigest()


def api(path):
    request = Request(API + path, headers={'Accept': 'application/vnd.github+json'})
    request.add_unredirected_header('Authorization', 'Bearer ' + os.environ['GH_TOKEN'])
    with urlopen(request, timeout=60) as response:
        return json.load(response)


def write(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + '\n')


def download(run, source, prefix, temporary):
    artifacts = api(f'/actions/runs/{run}/artifacts?per_page=100')['artifacts']
    candidates = [row for row in artifacts if row['name'].startswith(prefix)]
    assert len(candidates) == 1, f'ambiguous/missing artifact {run} {prefix}'
    artifact = candidates[0]
    assert not artifact['expired'] and artifact['workflow_run']['head_sha'] == source
    folder = OUT / 'downloads' / str(artifact['id'])
    folder.mkdir(parents=True, exist_ok=False)
    raw = folder / 'raw.zip'
    request = Request(API + f"/actions/artifacts/{artifact['id']}/zip")
    # Authorization applies only to api.github.com; redirects never inherit it.
    request.add_unredirected_header('Authorization', 'Bearer ' + os.environ['GH_TOKEN'])
    try:
        with urlopen(request, timeout=120) as response, raw.open('wb') as target:
            shutil.copyfileobj(response, target, length=1024 * 1024)
    except HTTPError as error:
        raise RuntimeError(f"artifact {artifact['id']} HTTP {error.code}") from None
    except URLError:
        raise RuntimeError(f"artifact {artifact['id']} transport failure") from None
    assert raw.stat().st_size == artifact['size_in_bytes']
    assert 'sha256:' + sha(raw) == artifact['digest'], 'original ZIP digest mismatch'
    extracted = temporary / 'original' / str(artifact['id'])
    extracted.mkdir(parents=True)
    files, names = [], set()
    with zipfile.ZipFile(raw) as archive:
        assert sum(row.file_size for row in archive.infolist()) <= 4 * 1024**3
        for info in archive.infolist():
            if info.is_dir():
                continue
            name = PurePosixPath(info.filename)
            assert not name.is_absolute() and '..' not in name.parts and '\\' not in str(name)
            assert str(name) not in names and ((info.external_attr >> 16) & 0o170000) != 0o120000
            names.add(str(name))
            path = extracted / str(name)
            path.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(info) as original, path.open('wb') as target:
                shutil.copyfileobj(original, target, length=1024 * 1024)
            assert path.stat().st_size == info.file_size
            files.append(dict(path=str(name), bytes=path.stat().st_size, sha256=sha(path)))
    write(folder / 'artifact.json', artifact)
    write(folder / 'manifest.json', dict(archive_bytes=raw.stat().st_size, archive_sha256=sha(raw), files=files))
    print(json.dumps(dict(downloaded=artifact['id'], source=source, files=len(files), archive_sha256=sha(raw))), flush=True)
    return extracted, artifact


def load(path):
    spec = importlib.util.spec_from_file_location('review_' + path.parent.name.replace('-', '_'), path)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


def source_tree(source, temporary):
    path = temporary / 'sources' / source
    if not path.exists():
        path.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(['git', 'worktree', 'add', '--detach', str(path), source],
                       check=True, stdout=subprocess.DEVNULL)
    return path


def input_hashes(header):
    folder = header / 'cache-range-binaries'
    build = json.loads((folder / 'build-metadata.json').read_text())
    assert build['variants']['after']['source'] == 'ba474745fae3c08332d9e0862d9d5650550664ff'
    result = {}
    for name, info in build['variants']['after']['binaries'].items():
        path = folder / 'after' / name
        assert path.stat().st_size == info['bytes'] and sha(path) == info['sha256']
        result[name] = sha(path)
    client = header / 'cache-range-private/protocol-state-client'
    result['protocol-state-client'] = sha(client)
    assert result == {
        'Zelvik-Edge': '28e988dbe83f90b9cd5e7f50f6346d0bd0293b4f320bca0a80bf0193fff4b12e',
        'issue34-controller': '1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2',
        'protocol-state-client': '368312b058eb6a98373260a6b4977beb1655744418406c7c1c1bcedce92b3c1b',
    }
    return result


def check_inputs(root, expected, tree):
    provenance = root / 'provenance.json'
    if provenance.exists():
        data = json.loads(provenance.read_text())
        for name, digest in data['expected_binaries'].items():
            assert expected[Path(name).name] == digest
    for path in root.glob('round-*/manifest.json'):
        manifest = json.loads(path.read_text())
        assert manifest['client_sha256'] == expected['protocol-state-client']
        revision = 'a19f6e20db7c8c4d279b92cdedbaf60e1c35e31a'
        assert manifest['client_source_revision'] == revision
        actual_source = subprocess.check_output(['git', 'show', revision + ':benches/support/protocol_state_client.c'], cwd=tree)
        assert hashlib.sha256(actual_source).hexdigest() == manifest['client_source_sha256']


def futex_groups(data):
    results = []
    for row in data['records']:
        groups = {}
        for chain in row['chains']:
            text = '\n'.join(chain['frames'])
            group = 'Mace checkpoint' if 'CheckpointTask' in text and 'mace' in text else (
                'Mace collector' if 'CollectorSignal' in text and 'mace' in text else (
                'Mace other' if 'mace' in text else (
                'Node reclaim' if 'memory_reclaim' in text else (
                'Tokio blocking' if 'runtime::blocking' in text else 'other'))))
            groups[group] = groups.get(group, 0) + chain['samples']
        assert sum(groups.values()) == row['samples']
        results.append(dict(round=row['round'], protocol=row['protocol'], samples=row['samples'],
            operations=row['operations'], groups=groups, span=row['event_timestamp_span_seconds']))
    return results


def progress_records(root):
    rows = []
    for path in sorted(root.glob('round-*/*.perf-load-window.json')):
        data = json.loads(path.read_text())
        assert data['complete'] and data['exit'] == 0
        observed = []
        for name, before, after in zip(data['client_record_paths'], data['bytes_before'], data['bytes_after']):
            assert after > before > 0
            with gzip.open(path.parent / (name + '.gz'), 'rb') as source:
                prefix = source.read(after)
            assert len(prefix) == after
            a, b = prefix[:before].count(b'\n'), prefix.count(b'\n')
            assert b > a > 0
            observed.append(dict(before=a, after=b))
        rows.append(dict(path=str(path.relative_to(root)), record_progress=observed,
            start=data['started_monotonic_ns'], stop=data['stopped_monotonic_ns']))
    assert len(rows) == 4
    return rows


def factorial(records):
    result = []
    comparisons = [('r0-w0', 'r64k-w0'), ('r0-w0', 'r0-w1460'),
                   ('r64k-w0', 'r64k-w1460'), ('r0-w1460', 'r64k-w1460'),
                   ('r0-w0', 'r64k-w1460')]
    for protocol in ['h1', 'h1-tls', 'h2']:
        for before, after in comparisons:
            left = sorted([r for r in records if r['protocol'] == protocol and r['target'] == before], key=lambda r: r['round'])
            right = sorted([r for r in records if r['protocol'] == protocol and r['target'] == after], key=lambda r: r['round'])
            assert len(left) == len(right) == 4
            deltas = {field: [(b[field] / a[field] - 1) * 100 for a, b in zip(left, right)]
                      for field in ['rps', 'cpu_usec_per_request', 'p99_ms']}
            result.append(dict(protocol=protocol, before=before, after=after,
                changes={name: dict(rounds=values, median=statistics.median(values),
                                    minimum=min(values), maximum=max(values)) for name, values in deltas.items()}))
    return result


def inspect(label, run, source, kind, summary_name, header_hashes, temporary, deadline):
    while True:
        metadata = api(f'/actions/runs/{run}')
        assert metadata['head_sha'] == source
        if metadata['status'] == 'completed':
            break
        assert time.monotonic() < deadline, f'run {run} did not finish before review deadline'
        print(json.dumps(dict(waiting_run=run, status=metadata['status'])), flush=True)
        time.sleep(30)
    original, artifact = download(run, source, kind + '-', temporary)
    write(OUT / 'runs' / label / 'run.json', metadata)
    if metadata['conclusion'] != 'success':
        raise RuntimeError(f'original {label} run {run} failed; artifact retained, no successful replay claimed')
    tree = source_tree(source, temporary)
    work = temporary / 'working' / label
    shutil.copytree(original, work)
    check_inputs(work, header_hashes, tree)
    source_path = tree / 'scripts/perf' / kind
    result = dict(label=label, run=run, source=source, artifact=artifact['id'],
                  original_conclusion=metadata['conclusion'], archive_sha256=artifact['digest'],
                  actual_fixed_edge_controller_client_bytes_verified=True)
    if kind == 'cross-product-profile':
        wrapper = load(tree / 'docs/evidence/cross-product-cpu-2026-11-04/verify.py')
        helpers = temporary / 'cross-helpers' / label
        helpers.mkdir(parents=True)
        shutil.copy2(tree / 'scripts/perf/http-state/verify.py', helpers / 'http_verify.py')
        shutil.copy2(tree / 'scripts/perf/summarize_architecture.py', helpers / 'profile_summary.py')
        rebuilt = json.loads(json.dumps(wrapper.inspect(work, helpers, False)))
        stored = json.loads((original / summary_name).read_text())
        assert stored['source'] == source and stored['complete_cells'] == rebuilt['complete_cells'] == 24
        assert stored['successful'] == rebuilt['successful'] and stored['samples'] == rebuilt['samples']
        assert stored['failed'] == stored['lost_samples'] == rebuilt['lost_samples'] == 0
        def key(row):
            return row['round'], row['target'], row['protocol'], row['cache']
        indexed = {key(row): row for row in stored['profiles']}
        for row in rebuilt['profiles']:
            old = indexed[key(row)]
            for name in ['sha256', 'sample_count', 'lost_samples', 'periods', 'groups', 'leaf_symbols']:
                assert row[name] == old[name]
        result.update(cells=24, successful=rebuilt['successful'], failed=0,
                      samples=rebuilt['samples'], lost_samples=0,
                      max_host_steal_fraction=rebuilt['max_host_steal_fraction'])
        write(OUT / 'runs' / label / 'recomputed.json', rebuilt)
    else:
        filename = 'suite.py' if kind in ['bare-hyper', 'pingora-buffers'] else 'evidence.py'
        module = load(source_path / filename)
        if kind in ['bare-hyper', 'pingora-buffers']:
            module.OUT, module.PRIVATE = work, work / 'records'
            build = json.loads((work / 'build-metadata.json').read_text())
            assert build['source'] == source and build['expected_inputs'] == header_hashes
            check_inputs(work / 'records', header_hashes, tree)
            for name, expected in build['sources'].items():
                assert sha(work / 'source' / name) == expected
            if kind == 'bare-hyper':
                assert sha(work / 'bare-pingora-proxy') == build['pingora_binary']['sha256'] == '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'
        else:
            module.PRIVATE = work
        module.verify()
        rebuilt = json.loads((work / summary_name).read_text())
        assert rebuilt == json.loads((original / summary_name).read_text()), 'source-pinned recomputation mismatch'
        write(OUT / 'runs' / label / 'recomputed.json', rebuilt)
        result.update({name: rebuilt[name] for name in ['cells', 'successful', 'failed']})
        for name in ['samples', 'lost_samples', 'max_host_steal_fraction', 'wrong_ca_controls', 'medians']:
            if name in rebuilt:
                result[name] = rebuilt[name]
        if kind == 'framework-profile':
            for path in work.glob('round-*-pingora/profile-executable'):
                assert sha(path) == '1a5439941c53dc32044b665013add230ac8a2062ab34e72f92dfde7799d093c2'
        if kind == 'framework-syscalls':
            for path in work.glob('round-*/*.trace/syscall-summary.json'):
                data = json.loads(path.read_text())
                assert set(map(str, data['pids'])) == set(data['pid_start_times'])
                assert all(value > 0 for value in data['pid_start_times'].values())
                assert all(r['pid'] in data['pids'] and r['tid'] > 0 and r['stage'] in ['before', 'after', 'detach']
                           for r in data['raced_thread_exits'])
        if kind == 'futex-sampled':
            result.update(samples=rebuilt['samples'], lost_samples=rebuilt['lost_samples'],
                          groups=futex_groups(rebuilt), no_cpu_or_per_response_attribution=True,
                          client_progress_verified=label == 'futex-progress')
            if label == 'futex-progress':
                result['progress'] = progress_records(work)
        if kind == 'pingora-buffers':
            result['paired_factorial'] = factorial(json.loads((work / 'verified-records.json').read_text()))
    write(OUT / 'runs' / label / 'verification.json', result)
    print('PERF_REVIEW_RESULT ' + json.dumps(result, separators=(',', ':')), flush=True)
    return result


def render_report(output, failures):
    lines = ['# Retained performance evidence review', '',
             'ZIP/API digests, source-pinned verifiers, fixed input ELFs and retained transfer records were checked. Original failures are never replaced by retries.', '']
    for result in output:
        lines += [f"## {result['label']}", '',
                  f"Run {result['run']}, source {result['source']}, cells {result['cells']}, successful {result['successful']}, failed {result['failed']}.", '']
        if 'medians' in result and all('rps' in row for row in result['medians']):
            lines += ['| protocol | target | RPS | CPU us/response | P99 ms | client CPU | proxy CPU | origin CPU |',
                      '|---|---|---:|---:|---:|---:|---:|---:|']
            for row in result['medians']:
                lines.append(f"| {row['protocol']} | {row['target']} | {row['rps']:.2f} | {row.get('cpu_usec_per_request', 0):.3f} | {row.get('p99_ms', 0):.3f} | {row.get('client_cpu_fraction', 0):.3f} | {row.get('proxy_cpu_fraction', 0):.3f} | {row.get('origin_cpu_fraction', 0):.3f} |")
            lines.append('')
        if 'groups' in result:
            lines.append(json.dumps(result['groups']))
            lines += ['', 'Short event windows identify callers only; event attributes do not establish delivery decimation or CPU/response percentages.', '']
    if failures:
        lines += ['## Failed reviews', '']
        for failure in failures:
            lines += [f"- {failure['label']}: {failure['error_type']}: {failure['error']}", '']
    report = '\n'.join(lines) + '\n'
    return report


def main():
    OUT.mkdir(parents=True, exist_ok=False)
    output = []
    failures = []
    deadline = time.monotonic() + 40 * 60
    with tempfile.TemporaryDirectory() as folder:
        temporary = Path(folder)
        header, _ = download(41757321970, '4171ea700386d8ed7c755255c7eeccb7e804d644', 'header-node-binaries-', temporary)
        inputs = input_hashes(header)
        for spec in SPECS:
            try:
                output.append(inspect(*spec, inputs, temporary, deadline))
            except Exception as error:
                failure = dict(label=spec[0], run=spec[1], source=spec[2],
                               error_type=type(error).__name__, error=str(error))
                failures.append(failure)
                write(OUT / 'runs' / spec[0] / 'review-failure.json', failure)
                traceback.print_exc()
                print('PERF_REVIEW_FAILURE ' + json.dumps(failure), flush=True)
    write(OUT / 'verification.json', dict(results=output, failures=failures, all_completed=not failures,
        scope='original archives, immutable verifier sources, actual input ELF bytes and complete retained HTTP records; no performance rerun or deployment'))
    report = render_report(output, failures)
    (OUT / 'report.md').write_text(report)
    with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as summary:
        summary.write(report)
    if failures:
        raise RuntimeError(f'{len(failures)} evidence reviews failed; original archives and per-stage failures retained')


if __name__ == '__main__':
    main()
