#!/usr/bin/env python3
"""Build and validate the fixed release-manifest.json.

The manifest is the cross-repository release index consumed by the license
backend. GitHub Releases carry the archives, GHCR carries the container image,
and this file joins both with the exact tag and source commit. Validation is
deliberately fail-closed: an asset whose filename cannot prove its
architecture, a digest that is not sha256, or a tag that does not match the
version fails the release instead of publishing unverifiable metadata.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

SCHEMA = 1

REPOSITORY_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
COMMIT_RE = re.compile(r"^[0-9a-f]{40}$")
DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
VERSION_RE = re.compile(r"^(\d{2})\.(1[0-2]|[1-9])\.([1-5])(?:\.([1-9][0-9]*))?$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

COMPONENT_IMAGES = {
    "Zelvik-Edge": "ghcr.io/zelvik-labs/zelvik-edge",
    "Zelvik-DNS": "ghcr.io/zelvik-labs/zelvik-dns",
    "Zelvik-Probe": "ghcr.io/zelvik-labs/zelvik-probe",
}

PROBE_ARCHITECTURES = frozenset(
    [
        "amd64",
        "arm64",
        "386",
        "armv5",
        "armv6",
        "armv7",
        "mips",
        "mipsle",
        "mips64",
        "mips64le",
        "riscv64",
    ]
)


def architecture_for(component: str, name: str) -> str:
    """Return the existing architecture label for an archive filename.

    These are the labels the license backend derives from the same filenames
    (`license/server/packages.ts` and `shared/package-architectures.ts`), so a
    manual manifest can never disagree with the portal's own attribution.
    """
    lower = name.lower()
    if component == "Zelvik-Edge":
        if "sse4" in lower:
            return "x86_64 SSE4.2"
        if "avx2" in lower:
            return "x86_64 AVX2"
        if "avx512" in lower:
            return "x86_64 AVX-512"
        if "neoverse" in lower:
            return "aarch64 Neoverse"
        if "arm64" in lower or "aarch64" in lower:
            return "aarch64"
        if "x64" in lower or "x86_64" in lower:
            return "x86_64"
    elif component == "Zelvik-DNS":
        match = re.search(r"-(x86_64|aarch64)-unknown-linux-(?:gnu|musl)\.tar\.gz$", lower)
        if match:
            return match.group(1)
    elif component == "Zelvik-Probe":
        match = re.search(r"-linux-([a-z0-9._-]+)\.tar\.gz$", lower)
        if match and match.group(1) in PROBE_ARCHITECTURES:
            return match.group(1)
    raise ValueError(f"{name}: cannot prove a {component} architecture from the filename")


def _sha256_and_size(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def _image_entry(spec: str, component: str, version: str) -> dict:
    image_component, separator, ref_and_digest = spec.partition("=")
    if not separator:
        raise ValueError(f"image {spec!r} must be COMPONENT=REF@DIGEST")
    if image_component != component:
        raise ValueError(
            f"image component {image_component!r} does not match release component {component!r}"
        )
    ref, at, digest = ref_and_digest.rpartition("@")
    if not at:
        raise ValueError(f"image {spec!r} has no @sha256 digest")
    expected = f"{COMPONENT_IMAGES[component]}:{version}"
    if ref != expected:
        raise ValueError(f"image reference {ref!r} must be {expected!r}")
    if not DIGEST_RE.fullmatch(digest):
        raise ValueError(f"image digest {digest!r} is not sha256:<64 lowercase hex>")
    return {"component": component, "image": ref, "digest": digest}


def build_manifest(
    *,
    repository: str,
    tag: str,
    version: str,
    source_commit: str,
    component: str,
    assets: list[str],
    images: list[str],
) -> dict:
    if component not in COMPONENT_IMAGES:
        raise ValueError(f"unsupported component {component!r}")
    match = VERSION_RE.fullmatch(version)
    if not match:
        raise ValueError(f"version {version!r} is not CalVer YY.M.W[.P]")
    if tag != f"v{version}":
        raise ValueError(f"tag {tag!r} must be v{version}")
    if not REPOSITORY_RE.fullmatch(repository):
        raise ValueError(f"repository {repository!r} must be owner/name")
    if not COMMIT_RE.fullmatch(source_commit):
        raise ValueError(f"sourceCommit {source_commit!r} is not a full 40-char commit")
    if not assets:
        raise ValueError("at least one asset is required")
    if not images:
        raise ValueError("at least one image is required")

    suite_version = f"{match.group(1)}.{match.group(2)}"
    seen: set[str] = set()
    asset_entries: list[dict] = []
    for raw in sorted(assets, key=lambda value: Path(value).name):
        path = Path(raw)
        name = path.name
        if not name.endswith(".tar.gz"):
            raise ValueError(f"asset {name!r} is not a .tar.gz archive")
        if name in seen:
            raise ValueError(f"duplicate asset name {name!r}")
        seen.add(name)
        if not path.is_file():
            raise ValueError(f"asset {path} does not exist")
        sha256, size = _sha256_and_size(path)
        if size <= 0:
            raise ValueError(f"asset {name!r} is empty")
        if not SHA256_RE.fullmatch(sha256):
            raise ValueError(f"asset {name!r} produced an invalid SHA-256")
        asset_entries.append(
            {
                "component": component,
                "name": name,
                "architecture": architecture_for(component, name),
                "os": "linux",
                "sha256": sha256,
                "sizeBytes": size,
            }
        )

    return {
        "schema": SCHEMA,
        "repository": repository,
        "tag": tag,
        "version": version,
        "sourceCommit": source_commit,
        "suiteVersion": suite_version,
        "assets": asset_entries,
        "images": [_image_entry(spec, component, version) for spec in images],
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True, help="owner/name, e.g. Zelvik-Labs/edge")
    parser.add_argument("--tag", required=True, help="release tag, e.g. v26.10.4.5")
    parser.add_argument("--version", required=True, help="product version without the v prefix")
    parser.add_argument("--source-commit", required=True, help="full 40-char git commit")
    parser.add_argument("--component", required=True, choices=sorted(COMPONENT_IMAGES))
    parser.add_argument("--assets", nargs="+", required=True, help="release .tar.gz files")
    parser.add_argument(
        "--image",
        action="append",
        required=True,
        help="COMPONENT=ghcr.io/...:VERSION@sha256:<64 hex>; repeat for more images",
    )
    parser.add_argument("--output", required=True, help="manifest path")
    args = parser.parse_args(argv)

    try:
        manifest = build_manifest(
            repository=args.repository,
            tag=args.tag,
            version=args.version,
            source_commit=args.source_commit,
            component=args.component,
            assets=args.assets,
            images=args.image,
        )
    except ValueError as error:
        print(f"release-manifest: error: {error}", file=sys.stderr)
        return 2

    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(
        json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    print(
        f"release-manifest: wrote {output} "
        f"({len(manifest['assets'])} assets, {len(manifest['images'])} images)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
