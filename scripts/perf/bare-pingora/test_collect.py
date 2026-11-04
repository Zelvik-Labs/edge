#!/usr/bin/env python3
"""A failed profiler must retain uncompressed transfers without exporting secrets."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]


class FailedTransferRetention(unittest.TestCase):
    def test_both_collectors_keep_precompression_failure(self):
        for collector in ['bare-pingora', 'cross-product-profile']:
            with self.subTest(collector=collector), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                filename = 'collect.py' if collector == 'bare-pingora' else 'evidence.py'
                script = root / 'scripts/perf' / collector / filename
                script.parent.mkdir(parents=True)
                shutil.copy2(ROOT / 'scripts/perf' / collector / filename, script)
                private = root / '.local' / (collector + '-private')
                private.mkdir(parents=True)
                raw = b'200\t1.1\t1\t1024\t0.001\t0.001\t0\t0\n'
                (private / 'failed.0.client.tsv').write_bytes(raw)
                (private / 'server.key').write_text('private key fixture')
                (private / 'capture.perf.data').write_bytes(b'stack memory fixture')
                command = ['python3', str(script)]
                if collector == 'cross-product-profile':
                    command.append('collect')
                subprocess.run(command, check=True)
                public = root / '.local' / (collector + '-ci')
                manifest = json.loads((public / 'retained-manifest.json').read_text())
                self.assertEqual(len(manifest['files']), 1)
                retained = manifest['files'][0]
                self.assertEqual((public / retained['path']).read_bytes(), raw)
                self.assertEqual(retained['bytes'], len(raw))
                self.assertEqual(retained['sha256'], hashlib.sha256(raw).hexdigest())
                self.assertFalse(list(public.rglob('*.key')))
                self.assertFalse(list(public.rglob('*.perf.data')))


if __name__ == '__main__':
    unittest.main()
