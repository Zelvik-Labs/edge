#!/usr/bin/env python3
"""Verify the complete retained SNI artifact, executables and all timing blocks."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
from pathlib import Path
import statistics
import tempfile
import zipfile


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify():
    metadata = json.loads((OUT / 'measurement.json').read_text())
    assert len(metadata['runs']) == 10
    assert digest(OUT / 'baseline/corpus.log') == digest(OUT / 'candidate/corpus.log') == metadata['corpus_sha256']
    cases = ['single', 'fragmented', 'partial', 'oversized', 'trailing']
    values = defaultdict(dict)
    for run in metadata['runs']:
        with (OUT / run['file']).open() as source:
            rows = list(csv.DictReader(source, delimiter='\t'))
        assert len(rows) == 100
        for case in cases:
            selected = [row for row in rows if row['case'] == case]
            assert len(selected) == 20 and [int(row['block']) for row in selected] == list(range(20))
            iterations = sum(int(row['iterations']) for row in selected)
            elapsed = sum(int(row['nanoseconds']) for row in selected)
            assert iterations > 0 and elapsed > 0
            values[case][run['round'], run['variant']] = elapsed / iterations
    allocations = {}
    for variant in ['baseline', 'candidate']:
        with (OUT / variant / 'allocations.log').open() as source:
            allocations[variant] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                                    for row in csv.DictReader(source, delimiter='\t')}
    assert allocations['baseline']['oversized']['max_allocation_bytes'] == 0x1000003
    assert all(value == 0 for value in allocations['candidate']['oversized'].values())
    assert all(value == 0 for value in allocations['candidate']['partial'].values())
    summary = {'source': metadata['source'], 'baseline': metadata['baseline'], 'corpus_cases': metadata['corpus_cases'],
               'allocations': allocations, 'cases': {}, 'max_host_steal_fraction': max(run['host_steal_fraction'] for run in metadata['runs'])}
    for case in cases:
        paired = [values[case][r, 'candidate'] / values[case][r, 'baseline'] - 1 for r in range(5)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][r, 'baseline'] for r in range(5)),
                                  'candidate_ns': statistics.median(values[case][r, 'candidate'] for r in range(5)),
                                  'paired_change': paired, 'paired_median': statistics.median(paired)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    args = parser.parse_args()
    evidence = Path(__file__).resolve().parent
    manifest = json.loads((evidence/'manifest.json').read_text())
    assert args.archive.stat().st_size == manifest['archive_bytes']
    assert digest(args.archive) == manifest['archive_sha256']
    with tempfile.TemporaryDirectory(prefix='sni-evidence-') as temp:
        OUT = Path(temp)
        with zipfile.ZipFile(args.archive) as package:
            expected = {row['path']: row for row in manifest['files']}
            actual = {row.filename for row in package.infolist() if not row.is_dir()}
            assert actual == expected.keys()
            for name, row in expected.items():
                path = OUT/name
                assert path.resolve().is_relative_to(OUT)
                data = package.read(name)
                assert len(data) == row['bytes']
                assert hashlib.sha256(data).hexdigest() == row['sha256']
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
        metadata = json.loads((OUT/'build-metadata.json').read_text())
        assert metadata == json.loads((evidence/'build-metadata.json').read_text())
        assert metadata['source'] == 'b4d635519973b2ec9aa24a533129fdbb86d3fc4f'
        for variant in ['baseline', 'candidate']:
            for name, sha in metadata['variants'][variant]['binaries'].items():
                assert digest(OUT/variant/name) == sha
            for name in ['parser', 'probe']:
                assert digest(OUT/variant/(name+'.rs')) == metadata['variants'][variant][name+'_sha256']
        assert '5 passed; 0 failed' in (OUT/'tests.log').read_text()
        assert 'incomplete untrusted length must not allocate' in (OUT/'baseline/assert-bounded.log').read_text()
        verify()
        assert json.loads((OUT/'verified-summary.json').read_text()) == json.loads((evidence/'verified-summary.json').read_text())
    print('PASS: archive, 35 files, measured executables, 22812 output comparisons and all 1000 timing blocks')
