#!/usr/bin/env python3
"""Test phase control without Docker: python3 -I scripts/test-orbstack-orchestration.py."""

from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("test-orbstack-container.sh")


class OrchestrationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zelvik-orchestration-test-")
        self.results = Path(self.directory.name)
        source = SCRIPT.read_text()
        self.functions = source[source.index("run_build() {"):source.index("rustc --version")]
        self.functions = self.functions.replace("/results/", str(self.results) + "/")

    def tearDown(self):
        self.directory.cleanup()

    def run_phases(self, commands, cleanup_status=0):
        script = (
            "set -euo pipefail\nOVERALL_STATUS=0\nLAST_CLEANUP_STATUS=0\n"
            f"cleanup() {{ return {cleanup_status}; }}\n"
            + self.functions + commands
        )
        return subprocess.run(["bash", "-c", script], text=True, capture_output=True)

    def test_successful_phases_return_zero(self):
        result = self.run_phases("run_verification check true\nexit \"$OVERALL_STATUS\"\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.results / "check-exit-status.txt").read_text(), "0\n")

    def test_failed_phase_does_not_skip_independent_phase(self):
        result = self.run_phases(
            "run_verification unit-tests false\n"
            "run_verification clippy true\nexit \"$OVERALL_STATUS\"\n"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual((self.results / "unit-tests-exit-status.txt").read_text(), "1\n")
        self.assertEqual((self.results / "clippy-exit-status.txt").read_text(), "0\n")
        self.assertTrue((self.results / "unit-tests-cleanup.log").is_file())

    def test_failed_prerequisite_stops_immediately(self):
        result = self.run_phases("run_build ebpf false\nrun_verification check true\n")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse((self.results / "check-exit-status.txt").exists())

    def test_cleanup_failure_stops_independent_phases(self):
        result = self.run_phases(
            "run_verification check true\nrun_verification clippy true\n", cleanup_status=3
        )
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertFalse((self.results / "clippy-exit-status.txt").exists())

    def test_later_success_cannot_erase_prior_failure(self):
        result = self.run_phases(
            "run_verification tests bash -c 'exit 7'\n"
            "run_verification check true\nexit \"$OVERALL_STATUS\"\n"
        )
        self.assertEqual(result.returncode, 7, result.stderr)


if __name__ == "__main__":
    unittest.main()
