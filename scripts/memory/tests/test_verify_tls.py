import unittest

from scripts.memory import verify_tls


class WriteoutTests(unittest.TestCase):
    def test_valid_writeout(self):
        parsed = verify_tls.parse_curl_writeout("2\t0\t200\t1024")
        self.assertEqual(parsed["http_version"], "2")
        self.assertEqual(parsed["ssl_verify_result"], 0)
        self.assertEqual(parsed["http_code"], 200)
        self.assertEqual(parsed["size_download"], 1024)

    def test_http1_fallback_is_preserved_for_the_caller_to_reject(self):
        parsed = verify_tls.parse_curl_writeout("1.1\t0\t200\t1024")
        self.assertEqual(parsed["http_version"], "1.1")

    def test_wrong_field_count_is_an_error(self):
        with self.assertRaises(verify_tls.TlsVerifyError):
            verify_tls.parse_curl_writeout("2\t0\t200")

    def test_non_numeric_is_an_error(self):
        with self.assertRaises(verify_tls.TlsVerifyError):
            verify_tls.parse_curl_writeout("2\tno\t200\t10")


class AvailabilityTests(unittest.TestCase):
    def test_curl_http2_capability_is_reported(self):
        # CI installs curl with nghttp2. If curl is absent or built without
        # HTTP/2, this is a hard failure: the soak cannot claim h2 verification.
        self.assertIsInstance(verify_tls.curl_supports_http2(), bool)

    def test_missing_ca_is_an_error_before_any_network_call(self):
        with self.assertRaises(verify_tls.TlsVerifyError):
            verify_tls.verify(host="localhost", port=1, ca_file="/nonexistent/ca.pem")


if __name__ == "__main__":
    unittest.main()
