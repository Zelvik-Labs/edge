import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys
import zipfile

base, output = map(Path, sys.argv[1:3])
source = '9239da7ccf39cd31dc5712fef5829e692d36a326'
manifest = json.loads((base/'manifest.json').read_text())
archive = base/'raw.zip'
assert hashlib.sha256(archive.read_bytes()).hexdigest() == manifest['archive_sha256']
assert archive.stat().st_size == manifest['archive_bytes']
with zipfile.ZipFile(archive) as z:
    assert {i.filename for i in z.infolist() if not i.is_dir()} == {f['path'] for f in manifest['files']}
    for entry in manifest['files']:
        data = z.read(entry['path'])
        assert len(data) == entry['bytes'] and hashlib.sha256(data).hexdigest() == entry['sha256']
        assert (base/'extracted'/entry['path']).read_bytes() == data
directory = base/'extracted'
metadata = json.loads((directory/'measurements/metadata.json').read_text())
records = json.loads((directory/'measurements/records.json').read_text())
summary = json.loads((directory/'measurements/summary.json').read_text())
assert metadata['source_commit'] == source and metadata['measurement_cpus'] == [metadata['allowed_cpus_before'][0]]
assert metadata['rustflags'] == '-C target-cpu=x86-64-v2'
assert metadata['rustc'].startswith('rustc 1.99.1 ')
assert metadata['rounds'] == 5 and metadata['capacities'] == [65_536, 1_677_721, 8_000_000]
assert len(records) == summary['records'] == 15 and summary['passed'] and summary['all_attempts_retained']
assert [(r['repeat'], r['capacity_requested']) for r in records] == [
    (repeat, cap) for repeat in range(1,6) for cap in metadata['capacities'][::1 if repeat%2 else -1]]
for path, expected in metadata['source_sha256'].items():
    assert hashlib.sha256(subprocess.check_output(['git', 'show', source+':'+path])).hexdigest() == expected, path
for file in ['src/firewall/bounded_map.rs', 'src/firewall/state.rs', 'src/memory_governor.rs']:
    assert subprocess.check_output(['git','show',source+':'+file]) == subprocess.check_output(['git','show','708785cfdc711e920a82699424d1d7648bebf378:'+file])
for row in records:
    cap, repeat = row['capacity_requested'], row['repeat']
    raw = json.loads((directory/f'measurements/r{repeat}-capacity{cap}.stdout.log').read_text())
    assert all(row[k] == v for k, v in raw.items())
    assert not (directory/f'measurements/r{repeat}-capacity{cap}.stderr.log').read_text()
    assert row['physical_slots_per_table'] == max(cap//64,64)*64
    assert row['tables'] == 7 and row['segments_per_table'] == 64
    assert row['slot_size_bytes'] > 0 and row['slot_size_bytes'] % row['slot_alignment_bytes'] == 0
    assert row['slot_payload_bytes_all_tables'] == row['physical_slots_per_table']*row['slot_size_bytes']*7
    assert row['page_size'] == metadata['page_size']
    assert row['rss_live_empty_delta_bytes'] == row['rss_empty']-row['rss_before']
    assert row['verified_hits'] == row['verified_misses'] == row['lookup_iterations'] == 200_000
    assert row['live_entries'] == 7 and len(row['first_write_ns']) == 7
    assert all(v > 0 for v in row['first_write_ns'])
    for field in ['construction_ns','empty_visit_ns','empty_retain_ns','empty_lookup_ns','hit_lookup_ns']:
        assert row[field] > 0
    assert row['cpu_before']['monotonic_ns'] < row['cpu_after']['monotonic_ns']
    key = 'cpu'+str(metadata['measurement_cpus'][0])
    delta = [b-a for a,b in zip(row['cpu_before']['proc'][key],row['cpu_after']['proc'][key])]
    assert all(n >= 0 for n in delta[:8]) and sum(delta[:8]) > 0
    assert row['steal_pct'] == delta[7]/sum(delta[:8])*100
    assert row['quality_flags'] == (['host_cpu_steal_over_1pct'] if row['steal_pct'] > 1 else [])
    assert row['child_user_seconds'] >= 0 and row['child_system_seconds'] >= 0
    assert row['memory_before']['MemAvailable'] >= 3*1024**3
for cap, fields in summary['capacities'].items():
    selected = [r for r in records if r['capacity_requested'] == int(cap)]
    assert len(selected) == 5
    for field, values in fields.items():
        assert values['all'] == [r[field] for r in selected]
        assert values['median'] == statistics.median(values['all'])
tests = (directory/'tests.log').read_text()
assert '7 passed; 0 failed; 0 ignored' in tests
assert 'could not compile' not in tests
assert 'Finished' in (directory/'clippy.log').read_text()
assert 'Finished' in (directory/'build.log').read_text()
analysis=[]
for cap in metadata['capacities']:
    rows=[r for r in records if r['capacity_requested']==cap]
    analysis.append({'capacity_requested':cap,'physical_slots_per_table':rows[0]['physical_slots_per_table'],
                     'slot_payload_mib':rows[0]['slot_payload_bytes_all_tables']/2**20,
                     'rss_delta_mib_median':statistics.median(r['rss_live_empty_delta_bytes']/2**20 for r in rows),
                     'construction_ms_median':statistics.median(r['construction_ns']/1e6 for r in rows),
                     'empty_visit_ms_median':statistics.median(r['empty_visit_ns']/1e6 for r in rows),
                     'empty_retain_ms_median':statistics.median(r['empty_retain_ns']/1e6 for r in rows),
                     'empty_lookup_ns_per_call_median':statistics.median(r['empty_lookup_ns']/r['lookup_iterations'] for r in rows),
                     'hit_lookup_ns_per_call_median':statistics.median(r['hit_lookup_ns']/r['lookup_iterations'] for r in rows),
                     'first_write_ns_all':[v for r in rows for v in r['first_write_ns']],
                     'first_write_ns_median':statistics.median(v for r in rows for v in r['first_write_ns'])})
result={'passed':True,'source_commit':source,'records_checked':15,'existing_production_tests':7,
        'slot_sizes_bytes_observed':sorted({r['slot_size_bytes'] for r in records}),
        'max_observed_steal_pct':max(r['steal_pct'] for r in records),
        'analysis':analysis,'archive_sha256':manifest['archive_sha256'],
        'scope':'independent raw records, summaries, source/entry/archive hashes; binary hash is producer metadata because artifact excludes executable; no whole-node attribution or gain claim'}
output.write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k!='analysis'}))
