import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from types import SimpleNamespace
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SCRIPT = Path(__file__).with_name('protocol_matrix.py')
spec = importlib.util.spec_from_file_location('protocol_matrix', SCRIPT)
matrix = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix)
PAYLOAD = b'edge-protocol-integrity\n' * 100


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', str(len(PAYLOAD)))
        self.end_headers()
        self.wfile.write(PAYLOAD)

    def log_message(self, *_):
        pass


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.worker = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.worker.start()
        self.endpoint = {'url': f'http://127.0.0.1:{self.server.server_port}/payload'}

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.worker.join()

    def test_real_http11_and_corrupt_body(self):
        self.assertEqual(matrix.verify('h1', self.endpoint, PAYLOAD)['negotiated'], '1.1')
        with self.assertRaisesRegex(RuntimeError, 'body mismatch'):
            matrix.verify('h1', self.endpoint, b'wrong payload')

    def test_http10_is_not_counted_as_http11(self):
        class OldHandler(Handler):
            protocol_version = 'HTTP/1.0'
        server = ThreadingHTTPServer(('127.0.0.1', 0), OldHandler)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            with self.assertRaisesRegex(RuntimeError, 'protocol/TLS/status mismatch'):
                matrix.verify('h1', {'url': f'http://127.0.0.1:{server.server_port}/'}, PAYLOAD)
        finally:
            server.shutdown()
            server.server_close()
            worker.join()

    def test_failed_endpoint_stays_failed_and_other_targets_continue(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/'payload').write_bytes(PAYLOAD)
            config = {'payload': str(root/'payload'), 'targets': {
                'valid': {'h1': self.endpoint},
                'broken': {'h1': {'url': 'http://127.0.0.1:0/'}},
                'explicit-capability': {'h3': {'unsupported': 'fixture has no QUIC listener'}}}}
            (root/'config.json').write_text(json.dumps(config))
            result = subprocess.run([sys.executable, '-I', str(SCRIPT), '--config',
                str(root/'config.json'), '--output', str(root/'result'), '--repeats', '1'],
                capture_output=True, text=True, timeout=30)
            self.assertEqual(result.returncode, 1, result.stderr)
            manifest = json.loads((root/'result/manifest.json').read_text())
            self.assertTrue(manifest['completed'])
            self.assertEqual({row['target']: row['status'] for row in manifest['cases']},
                {'valid': 'passed', 'broken': 'failed', 'explicit-capability': 'not-supported'})

    def test_native_run_rejects_multiple_targets_before_starting_load(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root/'config.json'
            config.write_text(json.dumps({'targets': {'edge': {}, 'nginx': {}}}))
            with self.assertRaisesRegex(ValueError, 'exactly one target'):
                matrix.run(SimpleNamespace(config=config, oha='/not-started'))


if __name__ == '__main__':
    unittest.main()
