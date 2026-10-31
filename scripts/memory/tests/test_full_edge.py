"""Unit tests for the full-Edge lifecycle wrapper (no Edge process spawned)."""

from __future__ import annotations

import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from scripts.memory import full_edge


class SoakOutputTests(unittest.TestCase):
    def test_child_report_does_not_pollute_wrapper_summary(self):
        code = """
import json, pathlib, sys
sys.path.insert(0, sys.argv[1])
from scripts.memory import full_edge
status, recovery = full_edge._run_soak_with_recovery(
    [sys.executable, '-I', '-c', 'import sys; print("child report"); sys.exit(int(sys.argv[1]))', sys.argv[2]],
    duration=1, recovery_seconds=0,
    mode_file=pathlib.Path('/unused'), status_url=None,
)
print(json.dumps({'status': status, 'recovery': recovery}))
"""
        for exit_code in (0, 7):
            with self.subTest(exit_code=exit_code):
                result = subprocess.run(
                    [sys.executable, "-I", "-c", code,
                     str(full_edge.REPO_ROOT), str(exit_code)],
                    capture_output=True, text=True, check=True,
                )
                self.assertEqual(
                    json.loads(result.stdout),
                    {"status": exit_code, "recovery": None},
                )
                self.assertIn("child report", result.stderr)


class ControllerReadyRewriteTests(unittest.TestCase):
    def test_flag_becomes_a_real_smoke_run(self):
        self.assertEqual(
            full_edge._rewrite_controller_ready(
                ["--controller-ready", "--edge-binary", "x"]
            ),
            ["smoke", "--edge-binary", "x"],
        )

    def test_explicit_smoke_is_unchanged(self):
        self.assertEqual(
            full_edge._rewrite_controller_ready(["smoke", "--controller-ready"]),
            ["smoke"],
        )

    def test_serve_cannot_claim_startup(self):
        with self.assertRaises(full_edge.FullEdgeError):
            full_edge._rewrite_controller_ready(
                ["serve", "--controller-ready", "--edge-binary", "x"]
            )


class ScopeTests(unittest.TestCase):
    def test_installed_home_is_refused(self):
        with self.assertRaises(full_edge.PrerequisiteError):
            full_edge._check_home(Path("/opt/zelvik/edge"))

    def test_unmarked_non_empty_home_is_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "api_node.yaml").write_text("secret: x\n", encoding="utf-8")
            with self.assertRaises(full_edge.PrerequisiteError):
                full_edge._check_home(Path(directory))

    def test_component_binary_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "bench-defense"
            binary.write_text("#!/bin/sh\nexit 0\n", encoding="ascii")
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with self.assertRaises(full_edge.PrerequisiteError):
                full_edge._resolve_edge(str(binary))


class CommandTests(unittest.TestCase):
    def test_controller_uses_the_clap_subcommand_name(self):
        command = full_edge._controller_command(
            Path("/fixture/controller"),
            home=Path("/fixture/home"),
            edge_arg=full_edge.DEFAULT_EDGE_ARG,
            tls_host="localhost",
        )
        self.assertIn("start-internal", command)
        self.assertNotIn("_StartInternal", command)

    def test_spawn_smoke_flags_are_explicit(self):
        command = full_edge._controller_command(
            Path("/fixture/controller"),
            home=Path("/fixture/home"),
            edge_arg="start-internal",
            tls_host="localhost",
            edge_binary=Path("/fixture/Zelvik-Edge"),
            spawn_edge=True,
            smoke=True,
            report_file=Path("/fixture/report.json"),
            telemetry_mode="unavailable",
            telemetry_store=Path("/fixture/store"),
        )
        self.assertIn("--spawn-edge", command)
        self.assertIn("--smoke", command)
        self.assertIn("--report-file", command)
        self.assertIn("--telemetry-mode", command)
        self.assertIn("--telemetry-store", command)

    def test_mode_file_and_store_are_forwarded(self):
        command = full_edge._controller_command(
            Path("/fixture/controller"),
            home=Path("/fixture/home"),
            edge_arg="start-internal",
            tls_host="localhost",
            telemetry_mode="unavailable",
            telemetry_mode_file=Path("/fixture/mode"),
            telemetry_store=Path("/fixture/store"),
        )
        joined = " ".join(command)
        self.assertIn("--telemetry-mode unavailable", joined)
        self.assertIn("--telemetry-mode-file /fixture/mode", joined)
        self.assertIn("--telemetry-store /fixture/store", joined)


class FullReportValidationTests(unittest.TestCase):
    def passing_report(self) -> dict:
        return {
            "schema": "issue34-full-edge-fixture/v1",
            "status": "pass",
            "scope": {
                "full_edge": True,
                "production_startup": True,
                "component_lane": False,
            },
            "node_health_observed": True,
            "config_applied": True,
            "edge_pid": 7,
            "pid_at_exit": 7,
            "counters": {
                "registerNode": 1,
                "getDesiredRelease": 1,
                "reportNodeHealth": 1,
            },
            "checks": {
                "checks": [
                    {
                        "name": "http1-post",
                        "ok": True,
                        "http_version": "1.1",
                        "http_code": 200,
                        "payload_sha256": "abc",
                    },
                    {
                        "name": "http2-post",
                        "ok": True,
                        "http_version": "2",
                        "http_code": 200,
                        "payload_sha256": "abc",
                    },
                    {"name": "telemetry-reject", "ok": True},
                    {
                        "name": "telemetry-retryable-recovery",
                        "ok": True,
                        "pending_before": 2,
                        "pending_after": 0,
                        "core_pending_after": 0,
                        "detail_pending_after": 0,
                        "max_sequence_before": 2,
                        "acked_sequence": 2,
                        "max_batch_bytes": 512,
                        "recovered": True,
                        "bounded_status": True,
                    },
                    {"name": "node-status", "ok": True},
                ]
            },
            "telemetry": {
                "served": False,
                "mode": "unimplemented",
                "expected_code_number": 12,
                "counters": {"pendingBatches": 0, "maxBatchBytes": 512},
            },
            "tls": {"host": "localhost", "port": 443, "ca": "ca", "cert": "cert"},
        }

    def test_complete_report_passes(self):
        self.assertEqual(full_edge._validate_full_report(self.passing_report()), [])

    def test_component_label_is_not_full_edge(self):
        report = self.passing_report()
        report["scope"] = {
            "full_edge": False,
            "production_startup": False,
            "component_lane": True,
        }
        self.assertIn("scope.full_edge is not true", full_edge._validate_full_report(report))

    def test_pid_restart_fails(self):
        report = self.passing_report()
        report["pid_at_exit"] = 8
        self.assertIn(
            "Edge PID changed during the run", full_edge._validate_full_report(report)
        )

    def test_missing_retryable_recovery_fails(self):
        report = self.passing_report()
        checks = report["checks"]["checks"]
        report["checks"]["checks"] = [
            check
            for check in checks
            if check["name"] != "telemetry-retryable-recovery"
        ]
        self.assertIn(
            "telemetry retry/recovery did not pass",
            full_edge._validate_full_report(report),
        )

    def test_unavailable_mode_accepts_unavailable_code(self):
        report = self.passing_report()
        report["telemetry"] = {
            "served": False,
            "mode": "unavailable",
            "expected_code_number": 14,
            "counters": {"pendingBatches": 3},
        }
        self.assertEqual(full_edge._validate_full_report(report), [])


class SummaryTests(unittest.TestCase):
    def test_report_summary_keeps_only_bounded_checks(self):
        report = {
            "status": "pass",
            "duration_seconds": 1.5,
            "edge_pid": 7,
            "checks": {
                "checks": [
                    {
                        "name": "http2-post",
                        "ok": True,
                        "http_version": "2",
                        "http_code": 200,
                        "payload_sha256": "abc",
                        "secret": "must not be echoed",
                    }
                ]
            },
        }
        summary = full_edge._report_summary(report)
        self.assertEqual(summary["status"], "pass")
        self.assertEqual(summary["edge_pid"], 7)
        self.assertEqual(summary["checks"][0]["http_version"], "2")
        self.assertNotIn("secret", summary["checks"][0])


if __name__ == "__main__":
    unittest.main()
