"""Independent recomputation of retained Criterion samples and conditions."""
import hashlib
import json
import math
from pathlib import Path
import statistics
import subprocess
import sys

root, output = map(Path, sys.argv[1:3])
expected_source, expected_opt_level = sys.argv[3:5]
metadata = json.loads((root/'metadata.json').read_text())
summary = json.loads((root/'summary.json').read_text())
assert metadata['source_commit'] == expected_source
assert metadata['exit_code'] == 0 and not metadata['dirty_files']
assert metadata['binaries_unchanged'] and metadata['sources_unchanged']
assert len(metadata['binaries']) == 1
assert len(metadata['build_profiles']) == 1
assert metadata['build_profiles'][0]['opt_level'] == expected_opt_level
assert metadata['rustflags'] == '-C target-cpu=x86-64-v2'
assert metadata['benchmark_cpu'] in metadata['available_cpus']
assert metadata['command'][:3] == ['taskset', '-c', str(metadata['benchmark_cpu'])]
for name, digest in metadata['sources'].items():
    relative = 'scripts/perf/udp-demux/run.py' if name.endswith('scripts/perf/udp-demux/run.py') else name
    source = subprocess.check_output(['git', 'show', expected_source+':'+relative])
    assert hashlib.sha256(source).hexdigest() == digest, relative
cases = {}
for path in (root/'criterion').glob('**/new/benchmark.json'):
    identity = json.loads(path.read_text())
    samples = json.loads(path.with_name('sample.json').read_text())
    estimate = json.loads(path.with_name('estimates.json').read_text())
    counts, times = samples['iters'], samples['times']
    assert len(counts) == len(times) == 30
    assert all(math.isfinite(value) and value > 0 for value in counts+times)
    per_iteration = [time/count for time, count in zip(times, counts)]
    mean = statistics.fmean(per_iteration)
    assert math.isclose(mean, estimate['mean']['point_estimate'], rel_tol=1e-12, abs_tol=1e-6)
    assert math.isclose(statistics.median(per_iteration), estimate['median']['point_estimate'], rel_tol=1e-12, abs_tol=1e-6)
    key = (identity['group_id'], identity['function_id'], identity['value_str'])
    assert key not in cases
    cases[key] = {'mean_ns': mean, 'min_sample_ns': min(per_iteration),
                  'max_sample_ns': max(per_iteration), 'sample_count': 30,
                  'iterations': sum(counts),
                  'raw_samples': str(path.with_name('sample.json').relative_to(root))}
expected = {(f'udp_demux/sites{sites}-policy{policy}', method, str(port))
            for sites in [1, 1000] for policy in [8443, 0]
            for method in ['sets', 'point'] for port in [443, 8443, 5300, 9999]}
assert set(cases) == expected
assert summary['passed'] and summary['functions'] == 32
assert len(summary['comparisons']) == 16
for group, method, port in sorted(cases):
    if method == 'point':
        cell = summary['comparisons'][group+'/'+port]
        old, new = cases[(group, 'sets', port)], cases[(group, 'point', port)]
        assert cell['sets'] == old and cell['point'] == new
        assert cell['change_pct'] == (new['mean_ns']/old['mean_ns']-1)*100
steal = {}
for cpu in metadata['available_cpus']:
    key = 'cpu'+str(cpu)
    delta = [end-start for start, end in zip(metadata['cpu_before'][key], metadata['cpu_after'][key])]
    total = sum(delta[:8])
    assert total > 0 and all(value >= 0 for value in delta[:8])
    steal[key] = delta[7]/total*100
flags = []
if steal['cpu'+str(metadata['benchmark_cpu'])] > 1:
    flags.append('benchmark_cpu_overall_steal_over_1pct')
result = {'passed': True, 'source_commit': expected_source, 'functions': 32,
          'raw_samples': 960, 'build_profiles': metadata['build_profiles'],
          'overall_cpu_steal_pct': steal, 'quality_flags': flags,
          'scope': 'sample/estimate/summary recomputation and source checks, not proxy RPS or per-function steal observations; no samples removed'}
output.write_text(json.dumps(result, indent=2)+'\n')
print(json.dumps(result))
