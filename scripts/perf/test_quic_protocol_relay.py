"""Real datagrams verify relay ownership, boundaries, order and spacing."""
import importlib.util
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location('quic_relay', Path(__file__).parent / 'quinn-interop/relay.py')
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class RelayTests(unittest.TestCase):
    def test_two_real_udp_flows_preserve_boundaries_and_paced_order(self):
        for interval in [0, 100]:
            with self.subTest(interval=interval), tempfile.TemporaryDirectory() as directory:
                server = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
                server.bind(('127.0.0.1', 0))
                server.settimeout(2)
                failures = []

                def echo():
                    try:
                        for _ in range(8):
                            data, peer = server.recvfrom(65536)
                            server.sendto(data, peer)
                    except Exception as error:
                        failures.append(error)

                worker = threading.Thread(target=echo, daemon=True)
                worker.start()
                output = Path(directory) / 'relay.json'
                clients = [socket.socket(socket.AF_INET, socket.SOCK_DGRAM) for _ in range(2)]
                try:
                    for client in clients:
                        client.bind(('127.0.0.1', 0))
                        client.settimeout(2)
                    with relay.UdpRelay(f'https://localhost:{server.getsockname()[1]}/hit/payload', interval, output) as forward:
                        target = forward.front.getsockname()
                        for index in range(4):
                            for flow, client in enumerate(clients):
                                client.sendto(bytes([flow, index]) * (32 + index * 400), target)
                        for flow, client in enumerate(clients):
                            for index in range(4):
                                data, peer = client.recvfrom(65536)
                                self.assertEqual(data, bytes([flow, index]) * (32 + index * 400))
                                self.assertEqual(peer, target)
                    worker.join(timeout=2)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(failures, [])
                    stats = json.loads(output.read_text())
                    self.assertIsNone(stats['error'])
                    self.assertEqual(stats['flows'], 2)
                    for counter in ['received_client', 'forwarded_client', 'received_server', 'forwarded_server']:
                        self.assertEqual(stats[counter], 8)
                    self.assertEqual(stats['queued_at_shutdown'], 0)
                    self.assertEqual(stats['bytes_at_shutdown'], 0)
                    self.assertGreaterEqual(stats['minimum_server_send_interval_ns'], interval * 1000)
                finally:
                    for client in clients:
                        client.close()
                    server.close()


if __name__ == '__main__':
    unittest.main()
