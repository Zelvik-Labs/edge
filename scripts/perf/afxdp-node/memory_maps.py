"""Post-timing Linux smaps observations, with raw evidence and process identity."""
import hashlib
from pathlib import Path
import re
import time
from thread_cpu import parse_stat

FIELDS = ['Size', 'Rss', 'Pss', 'Private_Clean', 'Private_Dirty',
          'Shared_Clean', 'Shared_Dirty', 'Anonymous', 'Swap']
HEADER = re.compile(r'^[0-9a-f]+-[0-9a-f]+\s+[-rwxps]{4}\s+[0-9a-f]+\s+[0-9a-f]+:[0-9a-f]+\s+\d+(?:\s+.*)?$')


def parse_smaps(text):
    mappings = []
    for line in text.splitlines():
        if HEADER.fullmatch(line):
            parts = line.split(None, 5)
            mappings.append({'address': parts[0], 'permissions': parts[1],
                             'name': parts[5] if len(parts) == 6 else '', 'bytes': {}})
        elif line.split(':', 1)[0] in FIELDS:
            assert mappings, 'smaps fields before a mapping header'
            name, value, unit = line.replace(':', '', 1).split()
            assert unit == 'kB' and int(value) >= 0, 'invalid smaps units or value'
            assert name not in mappings[-1]['bytes'], 'duplicate smaps field'
            mappings[-1]['bytes'][name] = int(value)*1024
    assert mappings, 'empty smaps observation'
    for mapping in mappings:
        assert set(mapping['bytes']) == set(FIELDS), 'incomplete smaps mapping'
    return {'mapping_count': len(mappings),
            'totals_bytes': {field: sum(m['bytes'][field] for m in mappings) for field in FIELDS},
            'largest_resident_mappings': sorted(mappings, key=lambda m: m['bytes']['Rss'], reverse=True)[:12]}


def capture(pid, directory):
    root = Path('/proc')/str(pid)
    before_ns = time.monotonic_ns()
    before = parse_stat((root/'stat').read_text())
    raw = (root/'smaps').read_bytes()
    after = parse_stat((root/'stat').read_text())
    after_ns = time.monotonic_ns()
    assert before['start_ticks'] == after['start_ticks'], 'memory observation process identity changed'
    result = parse_smaps(raw.decode())
    filename = f'smaps-{pid}.log'
    (directory/filename).write_bytes(raw)
    result.update(pid=pid, start_ticks=before['start_ticks'],
                  read_before_ns=before_ns, read_after_ns=after_ns,
                  raw_file=filename, raw_sha256=hashlib.sha256(raw).hexdigest(),
                  scope='after timed traffic and idle observation; sequential smaps, not an atomic allocation profile; process RSS sums can repeat shared pages')
    return result
