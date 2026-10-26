#!/usr/bin/env python3
"""Behaviour tests for scripts/release_manifest.py (fixtures only, no build)."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

# Keep the checkout clean when the release job runs this test.
sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[1]
_SPEC = importlib.util.spec_from_file_location(
    "release_manifest", ROOT / "scripts" / "release_manifest.py"
)
assert _SPEC and _SPEC.loader
release_manifest = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(release_manifest)

DIGEST = "sha256:" + "a" * 64
COMMIT = "b" * 40


class ReleaseManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.root = Path(self._tmp.name)

    def asset(self, name: str, data: bytes = b"package-bytes") -> str:
        path = self.root / name
        path.write_bytes(data)
        return str(path)

    def build(self, component: str, assets: list[str], images: list[str] | None = None,
              version: str = "26.10.4.5", tag: str | None = None,
              repository: str = "Zelvik-Labs/edge", source_commit: str = COMMIT) -> dict:
        return release_manifest.build_manifest(
            repository=repository,
            tag=tag or f"v{version}",
            version=version,
            source_commit=source_commit,
            component=component,
            assets=assets,
            images=images
            or [f"{component}=ghcr.io/zelvik-labs/{component.lower()}:{version}@{DIGEST}"],
        )

    def image(self, component: str, version: str = "26.10.4.5", digest: str = DIGEST) -> str:
        return f"{component}=ghcr.io/zelvik-labs/{component.lower()}:{version}@{digest}"

    def test_edge_manifest_has_the_fixed_schema_and_existing_arch_labels(self) -> None:
        assets = [
            self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz"),
            self.asset("Zelvik-Edge-linux-x64-v3-avx2.tar.gz"),
            self.asset("Zelvik-Edge-linux-x64-v4-avx512.tar.gz"),
            self.asset("Zelvik-Edge-linux-arm64-generic.tar.gz"),
            self.asset("Zelvik-Edge-linux-arm64-neoverse-n1.tar.gz"),
        ]
        manifest = self.build("Zelvik-Edge", assets)

        self.assertEqual(
            list(manifest),
            ["schema", "repository", "tag", "version", "sourceCommit", "suiteVersion", "assets", "images"],
        )
        self.assertEqual(manifest["schema"], 1)
        self.assertEqual(manifest["repository"], "Zelvik-Labs/edge")
        self.assertEqual(manifest["tag"], "v26.10.4.5")
        self.assertEqual(manifest["version"], "26.10.4.5")
        self.assertEqual(manifest["sourceCommit"], COMMIT)
        self.assertEqual(manifest["suiteVersion"], "26.10")
        self.assertEqual(
            [asset["architecture"] for asset in manifest["assets"]],
            [
                "aarch64",
                "aarch64 Neoverse",
                "x86_64 SSE4.2",
                "x86_64 AVX2",
                "x86_64 AVX-512",
            ],
        )
        for asset in manifest["assets"]:
            self.assertEqual(
                list(asset),
                ["component", "name", "architecture", "os", "sha256", "sizeBytes"],
            )
            self.assertEqual(asset["component"], "Zelvik-Edge")
            self.assertEqual(asset["os"], "linux")
            self.assertEqual(len(asset["sha256"]), 64)
            self.assertGreater(asset["sizeBytes"], 0)
        self.assertEqual(
            manifest["images"],
            [
                {
                    "component": "Zelvik-Edge",
                    "image": "ghcr.io/zelvik-labs/zelvik-edge:26.10.4.5",
                    "digest": DIGEST,
                }
            ],
        )

    def test_dns_and_probe_architectures_are_derived_from_filenames(self) -> None:
        dns = self.build(
            "Zelvik-DNS",
            [self.asset("zelvik-dns-x86_64-unknown-linux-gnu.tar.gz")],
            [self.image("Zelvik-DNS")],
            repository="Zelvik-Labs/dns",
        )
        self.assertEqual(dns["assets"][0]["architecture"], "x86_64")
        self.assertEqual(dns["images"][0]["image"], "ghcr.io/zelvik-labs/zelvik-dns:26.10.4.5")

        probe = self.build(
            "Zelvik-Probe",
            [
                self.asset("zelvik-probe-linux-amd64.tar.gz"),
                self.asset("zelvik-probe-linux-arm64.tar.gz"),
                self.asset("zelvik-probe-linux-armv7.tar.gz"),
            ],
            [self.image("Zelvik-Probe")],
            repository="Zelvik-Labs/probe",
        )
        self.assertEqual(
            [asset["architecture"] for asset in probe["assets"]],
            ["amd64", "arm64", "armv7"],
        )
        self.assertEqual(probe["images"][0]["image"], "ghcr.io/zelvik-labs/zelvik-probe:26.10.4.5")

    def test_asset_hashes_and_sizes_are_the_actual_file_bytes(self) -> None:
        path = self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz", b"0123456789")
        manifest = self.build("Zelvik-Edge", [path])
        asset = manifest["assets"][0]
        self.assertEqual(asset["sizeBytes"], 10)
        self.assertEqual(
            asset["sha256"], "84d89877f0d4041efb6bf91a16f0248f2fd573e6af05c19f96bedb9f882f7882"
        )

    def test_repository_version_tag_and_commit_are_strict(self) -> None:
        asset = [self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")]
        with self.assertRaisesRegex(ValueError, "must be v"):
            self.build("Zelvik-Edge", asset, tag="26.10.4.5")
        with self.assertRaisesRegex(ValueError, "CalVer"):
            self.build("Zelvik-Edge", asset, version="26.13.4.5")
        with self.assertRaisesRegex(ValueError, "40-char"):
            self.build("Zelvik-Edge", asset, source_commit="b" * 39)
        with self.assertRaisesRegex(ValueError, "owner/name"):
            self.build("Zelvik-Edge", asset, repository="edge")

    def test_images_must_match_component_version_and_sha256_digest(self) -> None:
        asset = [self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")]
        with self.assertRaisesRegex(ValueError, "must be"):
            self.build(
                "Zelvik-Edge",
                asset,
                images=["Zelvik-Edge=ghcr.io/zelvik-labs/zelvik-edge:26.10.4.4@" + DIGEST],
            )
        with self.assertRaisesRegex(ValueError, "sha256"):
            self.build(
                "Zelvik-Edge",
                asset,
                images=["Zelvik-Edge=ghcr.io/zelvik-labs/zelvik-edge:26.10.4.5@sha256:nothex"],
            )
        with self.assertRaisesRegex(ValueError, "does not match"):
            self.build(
                "Zelvik-Edge",
                asset,
                images=["Zelvik-Probe=ghcr.io/zelvik-labs/zelvik-edge:26.10.4.5@" + DIGEST],
            )

    def test_unprovable_architectures_and_duplicate_assets_fail(self) -> None:
        with self.assertRaisesRegex(ValueError, "architecture"):
            self.build(
                "Zelvik-Edge", [self.asset("Zelvik-Edge-linux-riscv64.tar.gz")]
            )
        path = self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")
        with self.assertRaisesRegex(ValueError, "duplicate"):
            self.build("Zelvik-Edge", [path, path])
        with self.assertRaisesRegex(ValueError, "does not exist"):
            self.build("Zelvik-Edge", [str(self.root / "missing.tar.gz")])
        with self.assertRaisesRegex(ValueError, "empty"):
            self.build(
                "Zelvik-Edge",
                [self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz", b"")],
            )

    def test_cli_writes_manifest_and_rejects_bad_input(self) -> None:
        asset = self.asset("Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")
        output = self.root / "release-manifest.json"
        self.assertEqual(
            release_manifest.main(
                [
                    "--repository",
                    "Zelvik-Labs/edge",
                    "--tag",
                    "v26.10.4.5",
                    "--version",
                    "26.10.4.5",
                    "--source-commit",
                    COMMIT,
                    "--component",
                    "Zelvik-Edge",
                    "--assets",
                    asset,
                    "--image",
                    self.image("Zelvik-Edge"),
                    "--output",
                    str(output),
                ]
            ),
            0,
        )
        parsed = json.loads(output.read_text())
        self.assertEqual(parsed["assets"][0]["name"], "Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz")
        self.assertTrue(output.read_text().endswith("\n"))
        self.assertEqual(
            release_manifest.main(
                [
                    "--repository",
                    "Zelvik-Labs/edge",
                    "--tag",
                    "v26.10.4.5",
                    "--version",
                    "26.10.4.5",
                    "--source-commit",
                    COMMIT,
                    "--component",
                    "Zelvik-Edge",
                    "--assets",
                    asset,
                    "--image",
                    "Zelvik-Edge=ghcr.io/zelvik-labs/zelvik-edge:26.10.4.5@sha256:bad",
                    "--output",
                    str(output),
                ]
            ),
            2,
        )


if __name__ == "__main__":
    unittest.main()
