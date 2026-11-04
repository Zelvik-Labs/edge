#!/usr/bin/env python3
"""Verify the exact relay copy artifact and all paired records without compiling."""
import argparse
from collections import defaultdict
import csv
import hashlib
import json
from pathlib import Path
import statistics
import tempfile
import zipfile
CASES = ['1KiB_full', '64KiB_full', '1MiB_full', '64KiB_short64']

def digest(path):
    sha = hashlib.sha256()
    with path.open('rb') as source:
        while block := source.read(1024 * 1024):
            sha.update(block)
    return sha.hexdigest()


def read_tsv(path):
    with path.open() as source:
        return list(csv.DictReader(source, delimiter='\t'))


def verify():
    metadata = json.loads((OUT / 'measurement.json').read_text())
    assert len(metadata['runs']) == 10
    allocations, controls = {}, {}
    for label in ['baseline', 'candidate']:
        for file, sha in metadata['variants'][label]['binaries'].items():
            assert digest(OUT / label / file) == sha
        for file, sha in metadata['variants'][label]['sources'].items():
            assert digest(OUT / label / file) == sha
        allocations[label] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                              for row in read_tsv(OUT / label / 'allocations.log')}
        controls[label] = {row['case']: {k: int(v) for k, v in row.items() if k != 'case'}
                           for row in read_tsv(OUT / label / 'controls.log')}
        assert len(controls[label]) == 4
    for case in CASES:
        assert allocations['baseline'][case]['allocations'] - allocations['candidate'][case]['allocations'] == 1
        assert allocations['baseline'][case]['reallocations'] == allocations['candidate'][case]['reallocations'] == 0
    for case, values in controls['candidate'].items():
        assert values['accepted'] == values['counter'] == values['flushed']
        assert controls['baseline'][case]['flushed'] == 0
    values = defaultdict(dict)
    for run in metadata['runs']:
        rows = read_tsv(OUT / run['file'])
        assert len(rows) == 80
        for case in CASES:
            selected = [row for row in rows if row['case'] == case]
            assert len(selected) == 20 and [int(row['block']) for row in selected] == list(range(20))
            iterations = sum(int(row['iterations']) for row in selected)
            elapsed = sum(int(row['nanoseconds']) for row in selected)
            assert iterations > 0 and elapsed > 0
            values[case][run['round'], run['variant']] = elapsed / iterations
    summary = {'source': metadata['source'], 'baseline': metadata['baseline'], 'allocations': allocations,
               'controls': controls, 'cases': {}, 'max_host_steal_fraction': max(run['host_steal_fraction'] for run in metadata['runs'])}
    for case in CASES:
        paired = [values[case][r, 'candidate'] / values[case][r, 'baseline'] - 1 for r in range(5)]
        summary['cases'][case] = {'baseline_ns': statistics.median(values[case][r, 'baseline'] for r in range(5)),
                                 'candidate_ns': statistics.median(values[case][r, 'candidate'] for r in range(5)),
                                 'paired_change': paired, 'paired_median': statistics.median(paired)}
    (OUT / 'verified-summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary, indent=2))


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive',type=Path)
    args=parser.parse_args()
    evidence=Path(__file__).resolve().parent
    manifest=json.loads((evidence/'manifest.json').read_text())
    assert args.archive.stat().st_size == manifest['archive_bytes']
    assert digest(args.archive) == manifest['archive_sha256']
    with tempfile.TemporaryDirectory(prefix='relay-evidence-') as temp:
        OUT=Path(temp)
        with zipfile.ZipFile(args.archive) as package:
            expected={row['path']:row for row in manifest['files']}
            assert {row.filename for row in package.infolist() if not row.is_dir()} == expected.keys()
            for name,row in expected.items():
                path=OUT/name
                assert path.resolve().is_relative_to(OUT)
                data=package.read(name)
                assert len(data) == row['bytes']
                assert hashlib.sha256(data).hexdigest() == row['sha256']
                path.parent.mkdir(parents=True,exist_ok=True)
                path.write_bytes(data)
        assert json.loads((OUT/'build-metadata.json').read_text()) == json.loads((evidence/'build-metadata.json').read_text())
        assert 'accepted partial writes must survive error or cancellation' in (OUT/'baseline/assert-accounting.log').read_text()
        verify()
        assert json.loads((OUT/'verified-summary.json').read_text()) == json.loads((evidence/'verified-summary.json').read_text())
    print('PASS: archive, 41 files, measured executables, four accounting controls and all 800 timing blocks')
