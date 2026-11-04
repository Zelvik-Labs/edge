#!/usr/bin/env python3
"""Recompute the frozen production CPU attribution from its complete artifact."""
import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import tempfile
import zipfile

EVIDENCE = Path(__file__).resolve().parent


def module(name):
    spec = importlib.util.spec_from_file_location(name, EVIDENCE / (name + '.py'))
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inspect(root):
    provenance = json.loads((root / 'provenance.json').read_text())
    expected = json.loads((EVIDENCE / 'provenance.json').read_text())
    assert provenance == expected
    manifest_path = root / 'profile/manifest.json'
    manifest = json.loads(manifest_path.read_text())
    assert manifest['source_revision'] == provenance['harness_source']
    assert provenance['binary_source'] == 'ba474745fae3c08332d9e0862d9d5650550664ff'
    for path, sha in manifest['binaries'].items():
        matches = [v for k, v in provenance['expected_binaries'].items() if path.endswith('/' + k)]
        assert matches == [sha]
    assert manifest['client_sha256'] == provenance['expected_binaries']['.local/http-state-ci/protocol-state-client']
    retained = json.loads((root / 'retained-manifest.json').read_text())['files']
    assert len(retained) == 193
    for entry in retained:
        path = root / entry['path']
        assert path.stat().st_size == entry['bytes'] and digest(path) == entry['sha256']
    rows = module('http_verify').validate(manifest_path, root)
    assert len(rows) == 18 and all(r['status'] == 'passed' and r['failed'] == 0 for r in rows)
    expected_grid = {(p, c, n) for p in ['h1', 'h1-tls', 'h2'] for c in ['off', 'hit', 'miss'] for n in [0, 1]}
    assert {(c['protocol'], c['cache'], c['repeat']) for c in manifest['cases']} == expected_grid
    summary = json.loads((root / 'verified-profiles.json').read_text())
    assert summary == json.loads((EVIDENCE / 'verified-profiles.json').read_text())
    assert sum(r['successful'] for r in rows) == summary['successful'] == 4961504
    assert sum(r['failed'] for r in rows) == summary['failed'] == 0
    assert max(r['max_host_steal_fraction'] for r in rows) == summary['max_host_steal_fraction'] == 0
    profiles = module('profile_summary')
    aggregates = defaultdict(lambda: {'samples': 0, 'groups': Counter(), 'symbols': Counter(), 'unknown_leaves': 0})
    for case, saved in zip(manifest['cases'], summary['profiles'], strict=True):
        path = root / 'profile' / (case['stem'] + '.stacks.txt')
        actual = profiles.profile(path)
        assert actual['lost_samples'] == 0
        for key in actual:
            if key != 'path':
                value = {str(k): v for k, v in actual[key].items()} if key == 'periods' else actual[key]
                assert value == saved[key], (case['stem'], key)
        assert (saved['protocol'], saved['cache'], saved['repeat']) == (case['protocol'], case['cache'], case['repeat'])
        assert (saved['successful'], saved['failed']) == (case['native']['successful'], case['native']['failed'])
        assert actual['periods'] == {5025125: actual['sample_count']}
        aggregate = aggregates[case['protocol'], case['cache']]
        aggregate['samples'] += actual['sample_count']
        aggregate['groups'].update({k: v['samples'] for k, v in actual['groups'].items()})
        for block in path.read_text().split('\n\n'):
            if 'cpu-clock:u:' not in block:
                continue
            block = re.sub(r'\[[0-9a-f]{8,}\]', '', block)
            lines = block.splitlines()[1:]
            aggregate['unknown_leaves'] += '[unknown]' in lines[0]
            symbols = set()
            for line in lines:
                if 'zelvik_edge::' in line:
                    symbols.add(re.sub(r'\+0x[0-9a-f]+.*', '', line.strip().split(' ', 1)[1]))
            aggregate['symbols'].update(symbols)
    total = sum(g['samples'] for g in aggregates.values())
    assert total == summary['total_samples'] == 43074
    output = {'successful': summary['successful'], 'failed': 0, 'samples': total,
              'lost_samples': 0, 'max_host_steal_fraction': 0, 'capacity_numbers_excluded': True,
              'inclusive_groups_overlap': True, 'binary_source': provenance['binary_source'], 'groups': []}
    for (protocol, cache), group in sorted(aggregates.items()):
        n = group['samples']
        output['groups'].append({'protocol': protocol, 'cache': cache, 'samples': n,
            'unknown_leaf_percent': group['unknown_leaves'] * 100 / n,
            'inclusive': {k: {'samples': v, 'percent': v * 100 / n} for k, v in group['groups'].items()},
            'edge_symbols': [{'symbol': k, 'samples': v, 'percent': v * 100 / n}
                             for k, v in sorted(group['symbols'].items(), key=lambda item: (-item[1], item[0]))[:50]]})
    return output

