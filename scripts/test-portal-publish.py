#!/usr/bin/env python3
"""Behaviour tests for scripts/portal-publish.py and release.yml.

The HTTP tests run a real local HTTP server and drive the real CLI as a
subprocess. They cover the failure modes that produced the empty 26.10.3.1
latest: zero/four receipts, mismatched digests, idempotent re-upload, missing
credentials, and the happy path where all five receipts and downloaded bytes
agree before promotion.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).resolve().parent.parent
CLI = ROOT / "scripts" / "portal-publish.py"
WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
VERSION = "26.10.3.1"
TAG = f"v{VERSION}"
ASSETS = [
    "Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz",
    "Zelvik-Edge-linux-x64-v3-avx2.tar.gz",
    "Zelvik-Edge-linux-x64-v4-avx512.tar.gz",
    "Zelvik-Edge-linux-arm64-generic.tar.gz",
    "Zelvik-Edge-linux-arm64-neoverse-n1.tar.gz",
]


class MockPortal:
    def __init__(self) -> None:
        self.releases: dict[str, dict] = {}
        self.packages: dict[tuple[str, str], bytes] = {}
        self.sessions: set[str] = set()
        self.update_keys: dict[str, str] = {}
        self.uploads: list[tuple[str, str, int]] = []
        self.downloads: list[str] = []
        self.set_latest_calls: list[str] = []
        self.next_token = 0
        self.port = 0

    def add_release(self, version: str, commit: str, assets: bool = False) -> dict:
        release = {
            "id": "rel_" + version.replace(".", "-"),
            "version": version,
            "channel": "stable",
            "gitCommit": commit,
            "is_latest_stable": False,
            "paused": False,
            "assets": {},
        }
        if assets:
            release["assets"] = {"Zelvik-Edge": []}
        self.releases[version] = release
        return release

    def asset_list(self, release: dict) -> dict:
        return {
            "Zelvik-Edge": [
                {
                    "name": name,
                    "component": "Zelvik-Edge",
                    "sha256": meta["sha256"],
                    "sizeBytes": meta["size"],
                    "hasFile": True,
                }
                for name, meta in release["assets"].get("Zelvik-Edge", {}).items()
            ]
        }

    def release_json(self, release: dict) -> dict:
        return {
            "id": release["id"],
            "version": release["version"],
            "channel": release["channel"],
            "gitCommit": release["gitCommit"],
            "is_latest_stable": release["is_latest_stable"],
            "is_latest_beta": False,
            "paused": release["paused"],
            "assets": self.asset_list(release),
        }


class Handler(BaseHTTPRequestHandler):
    server_version = "MockPortal/1.0"

    @property
    def state(self) -> MockPortal:
        return self.server.state  # type: ignore[attr-defined]

    def log_message(self, fmt: str, *args: object) -> None:
        pass

    def _json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length)

    def _auth_admin(self) -> bool:
        header = self.headers.get("Authorization", "")
        return header.startswith("Bearer ") and header[7:] in self.state.sessions

    def _auth_download(self) -> bool:
        header = self.headers.get("Authorization", "")
        return header.startswith("Bearer ") and header[7:] in self.state.update_keys

    def do_GET(self) -> None:
        parsed = urlparse(self.path)
        if parsed.path == "/api/v1/admin/releases":
            if not self._auth_admin():
                return self._json(401, {"error": "unauthorized"})
            return self._json(
                200,
                {"releases": [self.state.release_json(r) for r in self.state.releases.values()]},
            )
        match = re.fullmatch(r"/Download/([^/]+)/([^/]+)/([^/]+)", parsed.path)
        if match:
            if not self._auth_download():
                return self._json(401, {"error": "valid update key required"})
            version, _component, name = match.groups()
            release = self.state.releases.get(version)
            meta = (release or {}).get("assets", {}).get("Zelvik-Edge", {}).get(name)
            package = self.state.packages.get((version, name))
            if not release or not meta or package is None:
                return self._json(404, {"error": "payload not found"})
            self.state.downloads.append(name)
            self.send_response(200)
            self.send_header("Content-Type", "application/gzip")
            self.send_header("Content-Length", str(len(package)))
            self.send_header("X-Checksum-Sha256", meta["sha256"])
            self.end_headers()
            self.wfile.write(package)
            return
        return self._json(404, {"error": "not found"})

    def do_POST(self) -> None:
        parsed = urlparse(self.path)
        body = self._body()
        if parsed.path == "/api/v1/admin/login":
            payload = json.loads(body or b"{}")
            if payload.get("username") == "ci-admin" and payload.get("password") == "correct-password":
                self.state.next_token += 1
                token = f"session-token-{self.state.next_token}"
                self.state.sessions.add(token)
                return self._json(200, {"token": token})
            return self._json(401, {"error": "bad credentials"})
        if parsed.path == "/api/v1/admin/releases":
            if not self._auth_admin():
                return self._json(401, {"error": "unauthorized"})
            payload = json.loads(body)
            version = payload["version"]
            if version in self.state.releases:
                return self._json(409, {"error": "Release version already exists"})
            release = self.state.add_release(version, payload["git_commit"], assets=False)
            return self._json(201, {"success": True, "release_id": release["id"]})
        match = re.fullmatch(r"/api/v1/admin/releases/([^/]+)/set-latest", parsed.path)
        if match:
            if not self._auth_admin():
                return self._json(401, {"error": "unauthorized"})
            release = next(
                (r for r in self.state.releases.values() if r["id"] == match.group(1)), None
            )
            if not release:
                return self._json(404, {"error": "release not found"})
            for item in self.state.releases.values():
                item["is_latest_stable"] = False
            release["is_latest_stable"] = True
            self.state.set_latest_calls.append(release["version"])
            return self._json(200, {"success": True})
        if parsed.path == "/api/v1/admin/update-keys":
            if not self._auth_admin():
                return self._json(401, {"error": "unauthorized"})
            payload = json.loads(body)
            key = "zvu_" + hashlib.sha256((payload["name"] + str(len(self.state.update_keys))).encode()).hexdigest()
            key_id = "upk_" + str(len(self.state.update_keys) + 1)
            self.state.update_keys[key] = key_id
            return self._json(201, {"update_key": {"id": key_id, "name": payload["name"]}, "key": key})
        return self._json(404, {"error": "not found"})

    def do_PUT(self) -> None:
        parsed = urlparse(self.path)
        match = re.fullmatch(r"/api/v1/admin/releases/([^/]+)/assets/upload", parsed.path)
        if not match:
            return self._json(404, {"error": "not found"})
        if not self._auth_admin():
            return self._json(401, {"error": "unauthorized"})
        release = next(
            (r for r in self.state.releases.values() if r["id"] == match.group(1)), None
        )
        if not release:
            return self._json(404, {"error": "release not found"})
        query = parse_qs(parsed.query)
        name = query.get("name", [""])[0]
        component = query.get("component", [""])[0]
        if component != "Zelvik-Edge" or name not in ASSETS:
            return self._json(400, {"error": "bad component/name"})
        package = self._body()
        if not package:
            return self._json(400, {"error": "empty body"})
        sha = hashlib.sha256(package).hexdigest()
        release["assets"].setdefault("Zelvik-Edge", {})[name] = {
            "sha256": sha,
            "size": len(package),
        }
        self.state.packages[(release["version"], name)] = package
        self.state.uploads.append((release["version"], name, len(package)))
        return self._json(
            201,
            {
                "success": True,
                "asset": {
                    "component": "Zelvik-Edge",
                    "name": name,
                    "version": release["version"],
                    "sha256": sha,
                    "size_bytes": len(package),
                },
            },
        )

    def do_DELETE(self) -> None:
        parsed = urlparse(self.path)
        match = re.fullmatch(r"/api/v1/admin/update-keys/([^/]+)", parsed.path)
        if not match:
            return self._json(404, {"error": "not found"})
        if not self._auth_admin():
            return self._json(401, {"error": "unauthorized"})
        key_id = match.group(1)
        self.state.update_keys = {k: v for k, v in self.state.update_keys.items() if v != key_id}
        return self._json(200, {"success": True})


class PortalPublishTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.mkdtemp(prefix="portal-publish-test-")
        cls.packages = Path(cls.tmp) / "packages"
        cls.packages.mkdir()
        for index, name in enumerate(ASSETS):
            (cls.packages / name).write_bytes(f"package-{index}-{name}".encode() * 37)
        # The real workflow checks out the immutable tag, so HEAD there is the
        # tag commit. Tests run the CLI in a detached worktree at that tag
        # instead of assuming the developer's main checkout is at the tag.
        cls.source = Path(cls.tmp) / "tag-source"
        subprocess.run(
            ["git", "worktree", "add", "--detach", str(cls.source), TAG],
            cwd=ROOT, check=True, capture_output=True, text=True,
        )

    @classmethod
    def tearDownClass(cls) -> None:
        subprocess.run(
            ["git", "worktree", "remove", "--force", str(cls.source)],
            cwd=ROOT, capture_output=True, text=True,
        )
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def setUp(self) -> None:
        self.state = MockPortal()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.state = self.state  # type: ignore[attr-defined]
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.port = self.server.server_address[1]
        self.commit = subprocess.run(
            ["git", "rev-parse", f"{TAG}^{{commit}}"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout.strip()
        self.env = {
            "PATH": os.environ.get("PATH", ""),
            "HOME": os.environ.get("HOME", ""),
            "PORTAL_URL": f"http://127.0.0.1:{self.port}",
            "PORTAL_ADMIN_USER": "ci-admin",
            "PORTAL_ADMIN_PASSWORD": "correct-password",
            "GITHUB_ACTIONS": "",
        }
        self.receipts = Path(self.tmp) / f"receipts-{self._testMethodName}"
        self.receipts.mkdir(parents=True, exist_ok=True)

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def run_cli(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        merged = dict(self.env)
        if env:
            merged.update(env)
        return subprocess.run(
            [sys.executable, str(CLI), *args],
            cwd=self.source, env=merged, capture_output=True, text=True,
        )

    def prepare(self) -> None:
        result = self.run_cli(
            "prepare", "--tag", TAG, "--source-commit", self.commit, "--version", VERSION
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(VERSION, self.state.releases)

    def upload(
        self, name: str, package: Path | None = None, env: dict | None = None
    ) -> subprocess.CompletedProcess:
        return self.run_cli(
            "upload",
            "--package", str(package or (self.packages / name)),
            "--version", VERSION,
            "--source-commit", self.commit,
            "--receipt-dir", str(self.receipts),
            env=env,
        )

    def upload_all(self) -> None:
        for name in ASSETS:
            result = self.upload(name)
            self.assertEqual(result.returncode, 0, result.stderr)

    def promote(self) -> subprocess.CompletedProcess:
        return self.run_cli(
            "promote",
            "--receipts", str(self.receipts),
            "--version", VERSION,
            "--source-commit", self.commit,
        )

    def test_prepare_creates_then_reuses_same_tag_release(self) -> None:
        self.prepare()
        self.assertEqual(len(self.state.releases), 1)
        self.assertEqual(self.state.releases[VERSION]["gitCommit"], self.commit)
        second = self.run_cli(
            "prepare", "--tag", TAG, "--source-commit", self.commit, "--version", VERSION
        )
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(len(self.state.releases), 1)

    def test_prepare_rejects_existing_empty_release_from_other_commit(self) -> None:
        self.state.add_release(VERSION, "0" * 40, assets=False)
        result = self.run_cli(
            "prepare", "--tag", TAG, "--source-commit", self.commit, "--version", VERSION
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not the immutable tag commit", result.stderr)
        self.assertEqual(self.state.set_latest_calls, [])

    def test_upload_same_bytes_is_idempotent_and_different_bytes_refused(self) -> None:
        self.prepare()
        first = self.upload(ASSETS[0])
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(len(self.state.uploads), 1)
        second = self.upload(ASSETS[0])
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(len(self.state.uploads), 1)
        self.assertIn("idempotent", second.stdout)
        original_sha = self.state.releases[VERSION]["assets"]["Zelvik-Edge"][ASSETS[0]]["sha256"]
        hostile = Path(self.tmp) / "hostile.tar.gz"
        hostile.write_bytes(b"different bytes")
        # Same official name, different digest: must be rejected before any PUT.
        target = self.packages / ASSETS[0]
        backup = target.read_bytes()
        try:
            target.write_bytes(b"different bytes")
            refused = self.upload(ASSETS[0])
        finally:
            target.write_bytes(backup)
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("refusing to overwrite", refused.stderr)
        self.assertEqual(len(self.state.uploads), 1)
        self.assertEqual(
            self.state.releases[VERSION]["assets"]["Zelvik-Edge"][ASSETS[0]]["sha256"],
            original_sha,
        )

    def test_upload_missing_credentials_fails_closed(self) -> None:
        self.prepare()
        result = self.upload(ASSETS[0], env={"PORTAL_ADMIN_PASSWORD": ""})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("PORTAL_ADMIN_PASSWORD is not configured", result.stderr)
        self.assertEqual(self.state.uploads, [])

    def test_promote_zero_and_four_receipts_rejected(self) -> None:
        self.prepare()
        zero = self.promote()
        self.assertNotEqual(zero.returncode, 0)
        self.assertIn("0 receipts", zero.stderr)
        for name in ASSETS[:4]:
            self.assertEqual(self.upload(name).returncode, 0)
        four = self.promote()
        self.assertNotEqual(four.returncode, 0)
        self.assertIn("expected exactly 5", four.stderr)
        self.assertEqual(self.state.set_latest_calls, [])

    def test_promote_duplicate_receipt_rejected(self) -> None:
        self.prepare()
        self.upload_all()
        first = sorted(self.receipts.glob("*.json"))[0]
        shutil.copy(first, self.receipts / "duplicate.json")
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate receipt", result.stderr)
        self.assertEqual(self.state.set_latest_calls, [])

    def test_promote_altered_receipt_sha_rejected(self) -> None:
        self.prepare()
        self.upload_all()
        path = sorted(self.receipts.glob("*.json"))[0]
        payload = json.loads(path.read_text())
        payload["sha256"] = "f" * 64
        path.write_text(json.dumps(payload))
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("portal sha256", result.stderr)
        self.assertEqual(self.state.set_latest_calls, [])

    def test_promote_downloaded_byte_mismatch_rejected(self) -> None:
        self.prepare()
        self.upload_all()
        name = ASSETS[2]
        self.state.packages[(VERSION, name)] = b"tampered after upload"
        result = self.promote()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("authenticated download", result.stderr)
        self.assertEqual(self.state.set_latest_calls, [])

    def test_promote_five_matching_receipts_sets_latest(self) -> None:
        self.prepare()
        self.upload_all()
        result = self.promote()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.state.set_latest_calls, [VERSION])
        self.assertEqual(len(self.state.downloads), 5)
        self.assertTrue(self.state.releases[VERSION]["is_latest_stable"])
        self.assertEqual(self.state.update_keys, {})
        self.assertIn("promoted", result.stdout)
        token = next(iter(self.state.sessions))
        self.assertNotIn(token, result.stdout + result.stderr)


class WorkflowValidationTests(unittest.TestCase):
    def test_yaml_jobs_matrix_and_artifact_contract(self) -> None:
        try:
            import yaml
        except ImportError as exc:  # pragma: no cover - local pyyaml is present
            self.skipTest(f"PyYAML unavailable: {exc}")
        document = yaml.safe_load(WORKFLOW.read_text())
        on = document.get("on", document.get(True))
        self.assertIn("workflow_dispatch", on)
        self.assertIn("source_ref", on["workflow_dispatch"]["inputs"])
        self.assertEqual(on["workflow_dispatch"]["inputs"]["publish"]["type"], "boolean")
        self.assertIn("v*", on["push"]["tags"])
        jobs = document["jobs"]
        for job in ("prepare", "ebpf", "build", "release", "publish"):
            self.assertIn(job, jobs)
        self.assertEqual(jobs["ebpf"]["needs"], "prepare")
        self.assertEqual(jobs["build"]["needs"], "ebpf")
        self.assertIn("prepare", jobs["publish"]["needs"])
        self.assertIn("build", jobs["publish"]["needs"])
        self.assertIn("outputs.publish", jobs["publish"]["if"])
        matrix = jobs["build"]["strategy"]["matrix"]["include"]
        self.assertEqual(len(matrix), 5)
        self.assertEqual(
            {item["name"] for item in matrix},
            {
                "linux-x64-v2-sse4.2",
                "linux-x64-v3-avx2",
                "linux-x64-v4-avx512",
                "linux-arm64-generic",
                "linux-arm64-neoverse-n1",
            },
        )
        uploads = []

        def walk(node: object) -> None:
            if isinstance(node, dict):
                if str(node.get("uses", "")).startswith("actions/upload-artifact@"):
                    uploads.append(node)
                for value in node.values():
                    walk(value)
            elif isinstance(node, list):
                for value in node:
                    walk(value)

        walk(jobs)
        names = [item["with"]["name"] for item in uploads]
        self.assertEqual(len(names), len(set(names)), names)
        # Statically there are two upload steps: the shared eBPF object and the
        # per-leg receipt template. The template expands to five unique artifact
        # names because the matrix has five legs.
        self.assertEqual(sorted(names), ["ebpf-object", "receipt-${{ matrix.name }}"])
        self.assertEqual(len(matrix), 5)
        self.assertEqual(len({item["name"] for item in matrix}), 5)
        self.assertTrue(all(item["with"]["if-no-files-found"] == "error" for item in uploads))
        for item in uploads:
            path = str(item["with"]["path"])
            self.assertNotIn("dist", path)
            self.assertNotIn(".tar.gz", path)
        release_files = jobs["release"]["steps"][-1]["with"]["files"]
        self.assertNotIn("Zelvik-Edge-", release_files)
        self.assertIn("toa-main-source", release_files)
        self.assertIs(jobs["release"]["steps"][-1]["with"]["prerelease"], False)
        all_steps = json.dumps(jobs)
        self.assertIn("portal-publish.py", all_steps)
        self.assertIn("prepare", jobs["prepare"]["steps"][-1].get("run", ""))
        build_run = "\n".join(step.get("run", "") for step in jobs["build"]["steps"])
        self.assertIn("portal-publish.py", build_run)
        self.assertIn("upload", build_run)
        self.assertIn("--receipt-dir receipts", build_run)
        publish_run = "\n".join(step.get("run", "") for step in jobs["publish"]["steps"])
        self.assertIn("portal-publish.py promote", publish_run)
        # Only the small receipt JSON is uploaded by the matrix.
        for step in jobs["build"]["steps"]:
            if step.get("uses", "").startswith("actions/upload-artifact@"):
                self.assertIn("receipts", str(step["with"]["path"]))
                self.assertNotIn("dist", str(step["with"]["path"]))

    def test_run_blocks_are_bash_syntax_valid(self) -> None:
        try:
            import yaml
        except ImportError as exc:  # pragma: no cover
            self.skipTest(f"PyYAML unavailable: {exc}")
        document = yaml.safe_load(WORKFLOW.read_text())
        blocks: list[tuple[str, str]] = []
        for job_name, job in document["jobs"].items():
            for index, step in enumerate(job.get("steps", [])):
                if "run" in step:
                    blocks.append((f"{job_name}-{index}", step["run"]))
        self.assertGreater(len(blocks), 10)
        for label, block in blocks:
            # GitHub expressions are not shell; replace them with a token so
            # bash -n checks the surrounding shell grammar, not expressions.
            sanitized = re.sub(r"\$\{\{[^}]*\}\}", "EXPR", block)
            with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as handle:
                handle.write("#!/usr/bin/env bash\n" + sanitized + "\n")
                path = handle.name
            try:
                result = subprocess.run(["bash", "-n", path], capture_output=True, text=True)
            finally:
                os.unlink(path)
            self.assertEqual(result.returncode, 0, f"{label}: {result.stderr}")


if __name__ == "__main__":
    unittest.main(verbosity=2)
