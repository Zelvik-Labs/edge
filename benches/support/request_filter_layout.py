#!/usr/bin/env python3
"""Inspect the production async filter's ELF vtable layout, without running it.

This reports a type layout, not an allocation count or predicted throughput.
Require matching drop and poll relocations; do not infer a size from code bytes.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess


def inspect(path):
    data = path.read_bytes()
    if data[:6] != b'\x7fELF\x02\x01' or struct.unpack_from('<H', data, 18)[0] != 62:
        raise ValueError('requires little-endian x86-64 ELF')
    names = subprocess.check_output(['nm', '-S', '--defined-only', str(path)], text=True)
    names = subprocess.check_output(['c++filt', '-s', 'rust'], input=names, text=True)
    names = re.sub(r'\[[0-9a-f]{8,}\]', '', names)
    expected = '<zelvik_edge::proxy::EdgeProxy as pingora_proxy::proxy_trait::ProxyHttp>::request_filter::{closure#0}'
    symbols = {}
    for line in names.splitlines():
        parts = line.split(maxsplit=3)
        if len(parts) == 4 and parts[3] in [expected, 'core::ptr::drop_glue::<' + expected + '>']:
            symbols['poll' if parts[3] == expected else 'drop'] = {
                'address': int(parts[0], 16), 'code_bytes': int(parts[1], 16)}
    if set(symbols) != {'drop', 'poll'}:
        raise ValueError('unambiguous filter/drop symbols not found')
    relocations = {}
    for line in subprocess.check_output(['readelf', '-rW', str(path)], text=True).splitlines():
        match = re.match(r'([0-9a-f]+)\s+\S+\s+R_X86_64_RELATIVE\s+([0-9a-f]+)', line.strip())
        if match:
            relocations[int(match[1], 16)] = int(match[2], 16)
    phoff = struct.unpack_from('<Q', data, 32)[0]
    entry_size, count = struct.unpack_from('<HH', data, 54)
    segments = [struct.unpack_from('<IIQQQQQQ', data, phoff + i * entry_size) for i in range(count)]
    layouts = []
    for address, function in relocations.items():
        if function != symbols['drop']['address'] or relocations.get(address + 24) != symbols['poll']['address']:
            continue
        for kind, _, offset, virtual, _, size, _, _ in segments:
            if kind == 1 and virtual <= address and address + 32 <= virtual + size:
                location = offset + address - virtual
                size, alignment = struct.unpack_from('<QQ', data, location + 8)
                if not 0 < size < 2 ** 20 or alignment not in (1, 2, 4, 8, 16, 32, 64):
                    raise ValueError('invalid vtable layout')
                layouts.append({'vtable_address': address, 'file_offset': location,
                                'size_bytes': size, 'alignment': alignment})
    if not layouts or len({(r['size_bytes'], r['alignment']) for r in layouts}) != 1:
        raise ValueError('layout could not be uniquely established')
    return {'binary': str(path.resolve()), 'sha256': hashlib.sha256(data).hexdigest(),
            'symbols': symbols, 'layouts': layouts,
            'layout_is_not_allocation_count_or_throughput': True}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    options = parser.parse_args()
    options.output.write_text(json.dumps(inspect(options.binary), indent=2) + '\n')
