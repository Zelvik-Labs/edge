"""Recompute the same-run cgroup user/system costs without subtracting processes."""
from collections import defaultdict
import json
import statistics


def inspect(root):
    suite = json.loads((root / 'records/suite.json').read_text())
    rows = []
    groups = defaultdict(list)
    for run in suite['runs']:
        manifest = json.loads((root / 'records' / run['manifest']).read_text())
        assert run['exit'] == 0 and manifest['status'] == 'completed'
        for case in manifest['cases']:
            count = case['native']['successful']
            assert count > 0 and case['native']['failed'] == 0
            before, after = [case['proxy_' + side]['cpu.stat'] for side in ['before', 'after']]
            delta = {field: after[field] - before[field] for field in ['usage_usec', 'user_usec', 'system_usec']}
            assert all(value >= 0 for value in delta.values())
            row = dict(round=run['round'], target=run['target'], protocol=case['protocol'],
                       successful=count, **{key: value / count for key, value in delta.items()})
            rows.append(row)
            groups[run['target'], case['protocol']].append(row)
    assert len(rows) == 48
    medians = []
    for (target, protocol), data in sorted(groups.items()):
        assert len(data) == 4
        medians.append(dict(target=target, protocol=protocol,
            **{field: statistics.median(r[field] for r in data) for field in ['usage_usec', 'user_usec', 'system_usec']}))
    return {'scope': 'same-run proxy cgroup CPU per successful response; includes controller for Edge; medians need not add',
            'records': rows, 'medians': medians}
