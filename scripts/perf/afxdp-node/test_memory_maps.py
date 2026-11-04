import hashlib
import mmap
import os
from pathlib import Path
import tempfile
import unittest
from memory_maps import FIELDS, capture, parse_smaps


def mapping(header, amount):
    return header+'\n'+''.join(f'{field}: {amount} kB\n' for field in FIELDS)+'VmFlags: rd wr mr mw me ac sd\n'


class MemoryTests(unittest.TestCase):
    def test_byte_units_and_names_with_spaces_remain_inspectable(self):
        raw = mapping('1000-2000 rw-p 00000000 00:00 0', 4)
        raw += mapping('3000-5000 rw-s 00000000 00:01 23 /tmp/file with spaces (deleted)', 8)
        data = parse_smaps(raw)
        self.assertEqual(data['mapping_count'], 2)
        self.assertEqual(data['totals_bytes']['Rss'], 12*1024)
        self.assertEqual(data['largest_resident_mappings'][0]['name'], '/tmp/file with spaces (deleted)')
        self.assertEqual(data['largest_resident_mappings'][1]['name'], '')

    def test_empty_truncated_wrong_unit_duplicate_or_missing_fields_fail(self):
        valid = mapping('1000-2000 rw-p 00000000 00:00 0', 4)
        for raw in ['', 'Rss: 4 kB\n', valid.replace('Pss: 4 kB\n', ''),
                    valid.replace('Rss: 4 kB', 'Rss: 4 MB'), valid+'Rss: 4 kB\n']:
            with self.subTest(raw=raw), self.assertRaises(AssertionError):
                parse_smaps(raw)

    def test_real_resident_mapping_and_raw_hash_are_retained(self):
        size = 8*1024*1024
        with mmap.mmap(-1, size) as memory, tempfile.TemporaryDirectory() as tmp:
            for position in range(0, size, os.sysconf('SC_PAGE_SIZE')):
                memory[position] = 1
            result = capture(os.getpid(), Path(tmp))
            raw = (Path(tmp)/result['raw_file']).read_bytes()
            self.assertEqual(result['raw_sha256'], hashlib.sha256(raw).hexdigest())
            self.assertGreaterEqual(result['totals_bytes']['Rss'], size)
            self.assertGreater(result['mapping_count'], 0)
            self.assertLess(result['read_before_ns'], result['read_after_ns'])


if __name__ == '__main__':
    unittest.main()
