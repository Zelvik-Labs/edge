#!/usr/bin/env python3
"""Run with python3 -I scripts/test-clean-build-artifacts.py."""

import fcntl
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).with_name("clean-build-artifacts.py")
spec = importlib.util.spec_from_file_location("cleanup", SCRIPT)
cleanup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cleanup)


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zelvik-cleanup-test-")
        self.root = Path(self.directory.name).resolve()
        self.workspace = self.root / "edge"
        self.workspace.mkdir()
        (self.workspace / "Cargo.toml").write_text('[package]\nname="cleanup-test"\nversion="0.1.0"\n')
        self.target = self.workspace / "target"
        self.target.mkdir()
        (self.target / "artifact").write_bytes(b"build")

    def tearDown(self):
        self.directory.cleanup()

    def run_cleanup(self, *args, extra_env=None):
        env = dict(os.environ)
        for key in ("CARGO_TARGET_DIR", "ZELVIK_EDGE_MBX_GC", "MBX_TARGET_ROOT"):
            env.pop(key, None)
        env.update(extra_env or {})
        return subprocess.run(
            ["python3", "-I", str(SCRIPT), "--workspace", str(self.workspace), *args],
            env=env, text=True, capture_output=True,
        )

    def test_ordinary_target_cleanup(self):
        result = self.run_cleanup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.target.exists())

    def test_dry_run_keeps_artifacts(self):
        result = self.run_cleanup("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((self.target / "artifact").exists())

    def test_unsafe_target_paths_are_rejected(self):
        for target in (Path("/"), Path.home(), self.workspace, self.root, self.workspace / "src"):
            with self.subTest(target=target):
                with self.assertRaises(ValueError):
                    cleanup.validate_target(self.workspace, target)

    def test_outside_target_is_rejected(self):
        with self.assertRaises(ValueError):
            cleanup.validate_target(self.workspace, self.root / "other" / "target")

    def test_unmanaged_symlink_is_rejected(self):
        link = self.workspace / "other-target"
        link.symlink_to(self.root)
        with self.assertRaises(ValueError):
            cleanup.validate_target(self.workspace, link, self.root / "mbx" / "targets")

    def test_managed_symlink_is_identified(self):
        root = self.root / "mbx" / "targets"
        managed = root / "v1" / "checkout"
        managed.mkdir(parents=True)
        link = self.workspace / "managed-target"
        link.symlink_to(managed)
        self.assertEqual(cleanup.validate_target(self.workspace, link, root), (managed.resolve(), True))

    def test_active_cargo_build_is_kept(self):
        profile = self.target / "debug"
        profile.mkdir()
        lock = profile / ".cargo-lock"
        with lock.open("w+b") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            result = self.run_cleanup()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("build is active", result.stderr)
            self.assertTrue((self.target / "artifact").exists())

    def test_inactive_cargo_lock_does_not_block_cleanup(self):
        profile = self.target / "debug"
        profile.mkdir()
        (profile / ".cargo-lock").touch()
        result = self.run_cleanup()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.target.exists())

    def test_managed_ownership_record_is_required(self):
        with self.assertRaises(ValueError):
            cleanup.validate_managed_owner(self.workspace, self.target, self.root / "mbx-target")

    def test_other_workspace_record_is_rejected(self):
        managed = self.root / "mbx-target"
        managed.with_suffix(".json").write_text(json.dumps({
            "version": 1, "workspace_root": str(self.root / "other"),
        }))
        with self.assertRaises(ValueError):
            cleanup.validate_managed_owner(self.workspace, self.target, managed)

    def test_correct_workspace_record_is_accepted(self):
        managed = self.root / "mbx-target"
        managed.with_suffix(".json").write_text(json.dumps({
            "version": 1, "workspace_root": str(self.workspace),
        }))
        cleanup.validate_managed_owner(self.workspace, self.target, managed)

    def test_non_default_managed_view_is_rejected(self):
        with self.assertRaises(ValueError):
            cleanup.validate_managed_owner(
                self.workspace, self.workspace / "another-target", self.root / "mbx-target",
            )

    def test_mbx_failure_is_not_hidden(self):
        root = self.root / "mbx" / "targets"
        managed = root / "v1" / "checkout"
        managed.mkdir(parents=True)
        (managed / "artifact").write_bytes(b"build")
        self.target.rename(self.workspace / "ordinary-target")
        self.target.symlink_to(managed)
        managed.with_suffix(".json").write_text(json.dumps({
            "version": 1, "workspace_root": str(self.workspace), "updated_secs": 0,
        }))
        with patch.dict(os.environ, {"MBX_TARGET_ROOT": str(root)}, clear=True):
            with patch("sys.argv", [str(SCRIPT), "--workspace", str(self.workspace)]):
                with patch.object(cleanup.shutil, "which", return_value="mbx"):
                    with patch.object(cleanup.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "mbx")):
                        with self.assertRaises(subprocess.CalledProcessError):
                            cleanup.main()
        self.assertTrue((managed / "artifact").exists())


if __name__ == "__main__":
    unittest.main()
