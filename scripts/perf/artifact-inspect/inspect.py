#!/usr/bin/env python3
"""Read retained diagnostic text after verifying the original ZIP; execute no contents."""
import importlib.util
import json
import os
import hashlib
import shutil
import subprocess
from collections import Counter
from pathlib import Path
import tempfile

ROOT = Path.cwd()
spec = importlib.util.spec_from_file_location('archive_review', ROOT / 'scripts/perf/evidence-review/review.py')
review = importlib.util.module_from_spec(spec)
spec.loader.exec_module(review)
review.OUT = ROOT / '.local/artifact-inspect'
run = int(os.environ.get('INSPECT_RUN') or '41819185620')
source = os.environ.get('INSPECT_SOURCE') or '86d37d7d550dfa216f0764799ca2bb11d015f041'
assert len(source) == 40 and all(value in '0123456789abcdef' for value in source)
prefix = os.environ.get('INSPECT_PREFIX') or 'idle-cpu-'
with tempfile.TemporaryDirectory() as folder:
    extracted, artifact = review.download(run, source, prefix, Path(folder))
    print('ARTIFACT_INSPECT_META ' + json.dumps(dict(run=run, source=source, artifact=artifact['id'], digest=artifact['digest'])))
    seen = set()
    for path in sorted(extracted.rglob('*')):
        if not path.is_file() or not (path.name.endswith('.driver.log') or path.name == 'suite.json'):
            continue
        assert path.stat().st_size <= 1024 * 1024, 'diagnostic text exceeds bounded inspector budget'
        digest = review.sha(path)
        print('ARTIFACT_INSPECT_TEXT ' + json.dumps(dict(path=str(path.relative_to(extracted)), sha256=digest,
              duplicate=digest in seen, content=path.read_text() if digest not in seen else None)))
        seen.add(digest)

    metadata = review.api(f'/actions/runs/{run}')
    assert metadata['head_sha'] == source and metadata['status'] == 'completed'
    if metadata['conclusion'] == 'success' and prefix == 'idle-cpu-':
        tree = review.source_tree(source, Path(folder))
        work = Path(folder) / 'working'
        shutil.copytree(extracted, work)
        build = json.loads((work / 'build-metadata.json').read_text())
        assert build['source'] == source
        for name, expected in build['sources'].items():
            raw_source = subprocess.check_output(
                ['git', 'show', source + ':scripts/perf/idle-cpu/' + name], cwd=tree)
            assert hashlib.sha256(raw_source).hexdigest() == expected
        for path in (work / 'records').glob('round-*/manifest.json'):
            data = json.loads(path.read_text())
            assert data['driver_sha256'] == review.sha(tree / 'scripts/perf/protocol_state_matrix.py')
        header, header_artifact = review.download(41757321970,
            '4171ea700386d8ed7c755255c7eeccb7e804d644', 'header-node-binaries-', Path(folder))
        inputs = review.input_hashes(header)
        assert all(build['expected_inputs'][name] == expected for name, expected in inputs.items())
        verifier = review.load(tree / 'scripts/perf/idle-cpu/suite.py')
        verifier.OUT, verifier.PRIVATE = work, work / 'records'
        verifier.verify()
        for name in ['verified-summary.json', 'verified-records.json']:
            assert json.loads((work / name).read_text()) == json.loads((extracted / name).read_text())
        compact = []
        for row in json.loads((work / 'verified-records.json').read_text()):
            phases = {}
            for name in ['idle_before', 'loaded', 'idle_after']:
                phase = row[name]
                threads = Counter()
                for process in phase['process_cpu']:
                    for thread in process['stable_threads']:
                        threads[thread['comm']] += thread['user_ticks'] + thread['system_ticks']
                phases[name] = dict(seconds=phase['seconds'], cpu_usec=phase['cpu_usec'],
                    cpu_cores=phase['cpu_cores'], clock_ticks_per_second=phase['clock_ticks_per_second'],
                    process_cpu=[{key: value for key, value in item.items() if key != 'stable_threads'} for item in phase['process_cpu']],
                    stable_thread_ticks_by_comm=dict(threads),
                    stable_threads_are_not_complete_process_cpu=True)
            compact.append(dict(round=row['round'], target=row['target'], successful=row['successful'],
                failed=row['failed'], loaded_cpu_usec_per_response=row['loaded_cpu_usec_per_response'],
                max_host_steal_fraction=row['loaded_max_host_steal_fraction'],
                idle_memory_peak_bytes=row['idle_memory_peak_bytes'], phases=phases))
        result = dict(run=run, source=source, artifact=artifact['id'], archive_sha256=artifact['digest'],
            original_conclusion=metadata['conclusion'], actual_input_elf_bytes_verified=inputs,
            control_source_and_driver_bytes_verified=True,
            summary=json.loads((work / 'verified-summary.json').read_text()), windows=compact,
            no_idle_subtraction=True, no_capacity_ranking=True)
        review.write(review.OUT / 'idle-replay.json', result)
        print('IDLE_REPLAY_RESULT ' + json.dumps(result, separators=(',', ':')), flush=True)
