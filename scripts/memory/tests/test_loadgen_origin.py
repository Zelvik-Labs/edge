import hashlib
import http.client
import unittest

from scripts.memory import loadgen, origin


class LoadConfigTests(unittest.TestCase):
    def base(self, **overrides):
        values = {
            "sni": "localhost",
            "port": 8443,
            "ca_file": "/tmp/ca.pem",
            "concurrency": 4,
            "duration_seconds": 10,
        }
        values.update(overrides)
        return loadgen.LoadConfig(**values)

    def test_valid_config(self):
        self.base().validate()

    def test_bad_concurrency(self):
        with self.assertRaises(loadgen.LoadGenError):
            self.base(concurrency=0).validate()
        with self.assertRaises(loadgen.LoadGenError):
            self.base(concurrency=loadgen.MAX_CONCURRENCY + 1).validate()

    def test_post_requires_payload(self):
        with self.assertRaises(loadgen.LoadGenError):
            self.base(method="POST", payload=b"").validate()

    def test_reservoir_is_bounded(self):
        with self.assertRaises(loadgen.LoadGenError):
            self.base(latency_reservoir=1).validate()


class OriginTests(unittest.TestCase):
    def test_get_health_and_payload_and_post_echo(self):
        server = origin.OriginServer(port=0, payload_bytes=4096).start()
        try:
            conn = http.client.HTTPConnection("127.0.0.1", server.port, timeout=5)
            conn.request("GET", "/healthz")
            response = conn.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.read(), b"ok\n")

            conn.request("GET", "/payload")
            response = conn.getresponse()
            body = response.read()
            self.assertEqual(response.status, 200)
            self.assertEqual(body, origin.deterministic_payload(4096))
            conn.close()

            payload = b"issue34-echo\n" * 128
            conn = http.client.HTTPConnection("127.0.0.1", server.port, timeout=5)
            conn.request("POST", "/issue34/payload", body=payload)
            response = conn.getresponse()
            echoed = response.read()
            self.assertEqual(response.status, 200)
            self.assertEqual(hashlib.sha256(echoed).hexdigest(), hashlib.sha256(payload).hexdigest())
            conn.close()
        finally:
            server.stop()


if __name__ == "__main__":
    unittest.main()
