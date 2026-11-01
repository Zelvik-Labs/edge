import json
import os
import subprocess
import tempfile
import unittest

from scripts.memory import certs


class CertTests(unittest.TestCase):
    def test_generated_chain_verifies(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = certs.generate(tmp, hosts=("localhost", "edge.issue34.test"))
            for key in ("ca", "cert", "key"):
                self.assertTrue(os.path.isfile(manifest[key]), key)
            self.assertEqual(manifest["sni"], "localhost")
            result = subprocess.run(
                [
                    "openssl",
                    "verify",
                    "-CAfile",
                    manifest["ca"],
                    "-verify_hostname",
                    "localhost",
                    manifest["cert"],
                ],
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            with open(os.path.join(tmp, "manifest.json"), encoding="utf-8") as handle:
                self.assertEqual(json.load(handle)["sni"], "localhost")

    def test_empty_hosts_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(certs.CertError):
                certs.generate(tmp, hosts=())


if __name__ == "__main__":
    unittest.main()
