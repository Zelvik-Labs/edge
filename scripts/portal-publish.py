#!/usr/bin/env python3
"""Publish Zelvik-Edge release packages to license.zelvik.net.

The binary packages are distributed ONLY by the license portal. This tool has
three fail-closed stages:

  prepare  verify the tag/source commit/version are immutable and create (or
           validate) the portal release row.
  upload   upload one matrix leg's package with authenticated HTTP, refuse to
           overwrite a different payload, and emit a receipt.
  promote  require exactly the five official receipts, cross-check portal
           metadata AND the bytes actually downloaded over authenticated HTTP,
           and only then call set-latest.

No credential is ever put in a shell printf or printed. Request bodies are
built as JSON structures through json.dumps; the session token is only ever
written as a GitHub ::add-mask:: command line.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

COMPONENT = "Zelvik-Edge"
EXPECTED_ASSETS = (
    "Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz",
    "Zelvik-Edge-linux-x64-v3-avx2.tar.gz",
    "Zelvik-Edge-linux-x64-v4-avx512.tar.gz",
    "Zelvik-Edge-linux-arm64-generic.tar.gz",
    "Zelvik-Edge-linux-arm64-neoverse-n1.tar.gz",
)
RECEIPT_KEYS = {"name", "component", "version", "size", "sha256", "source_commit"}
STABLE_TAG = re.compile(r"^v[0-9]{2}\.(1[0-2]|[1-9])\.([1-5])(\.[1-9][0-9]*)?$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
GIT_COMMIT = re.compile(r"^[0-9a-f]{40}$")
SAFE_SEGMENT = re.compile(r"^[A-Za-z0-9._+~-]+$")

# Cloudflare's managed/bot rules answer the Python-urllib default User-Agent
# with HTTP 403 "error code: 1010" before the request reaches the license site
# (run 39613697640). Every portal request therefore carries this fixed client
# identity. It is a plain header, not a credential, and TLS verification stays
# fully enabled.
PORTAL_USER_AGENT = "Zelvik-Release/1.0 (+https://github.com/Zelvik-Labs/edge)"

GITHUB_ACTIONS = os.environ.get("GITHUB_ACTIONS") == "true"
_SECRETS: list[str] = []


class PortalError(RuntimeError):
    """A portal/HTTP operation failed or returned an unexpected payload."""


def _mask(secret: str) -> None:
    if not secret:
        return
    _SECRETS.append(secret)
    if GITHUB_ACTIONS:
        # This is the only place a token reaches stdout. GitHub removes the
        # value from the log after interpreting the command.
        print(f"::add-mask::{secret}", flush=True)


def _redact(text: str) -> str:
    for secret in _SECRETS:
        if secret:
            text = text.replace(secret, "***")
    return text


def fail(message: str) -> "None":
    message = _redact(str(message))
    if GITHUB_ACTIONS:
        print(f"::error::{message}", file=sys.stderr, flush=True)
    else:
        print(f"error: {message}", file=sys.stderr, flush=True)
    raise SystemExit(1)


def _env(name: str) -> str:
    value = os.environ.get(name, "")
    if not value and name.startswith("PORTAL_"):
        fail(f"{name} is not configured; refusing to skip publication")
    return value


def _git(*args: str) -> str:
    try:
        return subprocess.run(
            ["git", *args], check=True, capture_output=True, text=True
        ).stdout.strip()
    except subprocess.CalledProcessError as exc:
        fail(f"git {' '.join(args)} failed: {exc.stderr.strip()}")


def _sha256_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
            size += len(chunk)
    return digest.hexdigest(), size


def _write_json_atomic(path: Path, payload: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, indent=2, sort_keys=True)
            handle.write("\n")
        os.replace(tmp, path)
    finally:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass


def _cloudflare_hint(status: int, detail: str) -> str:
    """Explain a Cloudflare client-signature block instead of blaming credentials."""
    if status == 403 and "1010" in detail:
        return (
            " (likely a Cloudflare client-signature block: error code 1010, not a "
            "credential failure; the request was sent with User-Agent "
            f"{PORTAL_USER_AGENT!r})"
        )
    return ""


class Portal:
    def __init__(self, base_url: str, timeout: int = 180) -> None:
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self._token: str | None = None

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
        auth: bool = True,
        expected: tuple[int, ...] = (200,),
    ) -> tuple[int, bytes, dict[str, str]]:
        request_headers = dict(headers or {})
        request_headers["User-Agent"] = PORTAL_USER_AGENT
        if auth:
            token = self._token or self.login()
            request_headers["Authorization"] = f"Bearer {token}"
        request = urllib.request.Request(
            self.base_url + path, data=body, method=method, headers=request_headers
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                status = response.status
                payload = response.read()
                response_headers = dict(response.headers)
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", "replace")[:2000]
            raise PortalError(
                f"{method} {path} returned HTTP {exc.code}: {detail}"
                f"{_cloudflare_hint(exc.code, detail)}"
            ) from exc
        except urllib.error.URLError as exc:
            raise PortalError(f"{method} {path} failed: {exc.reason}") from exc
        if status not in expected:
            raise PortalError(
                f"{method} {path} returned HTTP {status}: "
                f"{payload.decode('utf-8', 'replace')[:2000]}"
            )
        return status, payload, response_headers

    def login(self) -> str:
        username = _env("PORTAL_ADMIN_USER")
        password = _env("PORTAL_ADMIN_PASSWORD")
        body = json.dumps({"username": username, "password": password}).encode()
        status, payload, _ = self._request(
            "POST",
            "/api/v1/admin/login",
            body=body,
            headers={"Content-Type": "application/json"},
            auth=False,
            expected=(200,),
        )
        try:
            token = json.loads(payload)["token"]
        except (KeyError, json.JSONDecodeError) as exc:
            raise PortalError("login response did not contain a token") from exc
        if not isinstance(token, str) or not token:
            raise PortalError("login response token is empty")
        self._token = token
        _mask(token)
        return token

    def releases(self) -> list[dict]:
        _, payload, _ = self._request("GET", "/api/v1/admin/releases")
        try:
            releases = json.loads(payload)["releases"]
        except (KeyError, json.JSONDecodeError) as exc:
            raise PortalError("releases response is not a JSON release list") from exc
        if not isinstance(releases, list):
            raise PortalError("releases response is not a list")
        return releases

    def release(self, version: str) -> dict | None:
        for release in self.releases():
            if str(release.get("version", "")).lstrip("v") == version.lstrip("v"):
                return release
        return None

    def create_release(self, version: str, source_commit: str) -> bool:
        body = json.dumps(
            {
                "version": version,
                "channel": "stable",
                "summary": f"Zelvik Edge {version}",
                "git_commit": source_commit,
                "components": [COMPONENT],
                "changelog": [],
            }
        ).encode()
        try:
            self._request(
                "POST",
                "/api/v1/admin/releases",
                body=body,
                headers={"Content-Type": "application/json"},
                expected=(201,),
            )
            return True
        except PortalError as exc:
            text = str(exc)
            if "HTTP 409" in text:
                return False
            raise
        return False

    def upload_asset(self, release_id: str, name: str, path: Path) -> dict:
        query = urllib.parse.urlencode(
            {
                "component": COMPONENT,
                "name": name,
                "asset_type": "archive",
                "os": "linux",
            }
        )
        payload = path.read_bytes()
        _, body, _ = self._request(
            "PUT",
            f"/api/v1/admin/releases/{urllib.parse.quote(release_id)}/assets/upload?{query}",
            body=payload,
            headers={"Content-Type": "application/octet-stream"},
            expected=(201,),
        )
        try:
            return json.loads(body)["asset"]
        except (KeyError, json.JSONDecodeError) as exc:
            raise PortalError("upload response did not contain asset metadata") from exc

    def create_update_key(self, name: str) -> tuple[str, str]:
        body = json.dumps({"name": name}).encode()
        _, payload, _ = self._request(
            "POST",
            "/api/v1/admin/update-keys",
            body=body,
            headers={"Content-Type": "application/json"},
            expected=(201,),
        )
        try:
            data = json.loads(payload)
            key_id = data["update_key"]["id"]
            key = data["key"]
        except (KeyError, TypeError, json.JSONDecodeError) as exc:
            raise PortalError("update-key response is incomplete") from exc
        _mask(key)
        return str(key_id), str(key)

    def delete_update_key(self, key_id: str) -> None:
        self._request(
            "DELETE",
            f"/api/v1/admin/update-keys/{urllib.parse.quote(key_id)}",
            expected=(200,),
        )

    def set_latest(self, release_id: str) -> None:
        self._request(
            "POST",
            f"/api/v1/admin/releases/{urllib.parse.quote(release_id)}/set-latest",
            expected=(200,),
        )

    def download(self, version: str, name: str, key: str, destination: Path) -> str:
        query = urllib.parse.quote(version)
        component = urllib.parse.quote(COMPONENT)
        filename = urllib.parse.quote(name)
        request = urllib.request.Request(
            f"{self.base_url}/Download/{query}/{component}/{filename}",
            headers={
                "Authorization": f"Bearer {key}",
                "User-Agent": PORTAL_USER_AGENT,
            },
            method="GET",
        )
        digest = hashlib.sha256()
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                if response.status != 200:
                    raise PortalError(f"download {name} returned HTTP {response.status}")
                header_sha = response.headers.get("X-Checksum-Sha256")
                with destination.open("wb") as handle:
                    for chunk in iter(lambda: response.read(1024 * 1024), b""):
                        digest.update(chunk)
                        handle.write(chunk)
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", "replace")[:2000]
            raise PortalError(
                f"download {name} returned HTTP {exc.code}: {detail}"
                f"{_cloudflare_hint(exc.code, detail)}"
            ) from exc
        except urllib.error.URLError as exc:
            raise PortalError(f"download {name} failed: {exc.reason}") from exc
        actual = digest.hexdigest()
        if header_sha and header_sha.lower() != actual:
            raise PortalError(
                f"authenticated download of {name} header sha256 {header_sha} "
                f"!= actual {actual}"
            )
        return actual


def _assert_tag_source(tag: str, source_commit: str) -> str:
    if not STABLE_TAG.fullmatch(tag):
        fail(f"tag {tag!r} is not a stable vYY.M.W[.P] release tag; beta/rc tags are not published")
    if not GIT_COMMIT.fullmatch(source_commit):
        fail(f"source commit {source_commit!r} is not a full 40-char git sha")
    head = _git("rev-parse", "HEAD")
    if head != source_commit:
        fail(f"checked-out HEAD {head} != requested source commit {source_commit}")
    tag_commit = _git("rev-parse", "--verify", f"refs/tags/{tag}^{{commit}}")
    if tag_commit != source_commit:
        fail(
            f"tag {tag} resolves to {tag_commit}, not the requested source commit "
            f"{source_commit}; an immutable tag must not be moved"
        )
    version = _git_version()
    if version != tag[1:]:
        fail(f"manifest version {version} != tag {tag}")
    manifest_tag = subprocess.run(
        ["bash", "scripts/release-version.sh", "--tag"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if manifest_tag != tag:
        fail(f"manifest release_tag {manifest_tag} != tag {tag}")
    return version


def _git_version() -> str:
    try:
        return subprocess.run(
            ["bash", "scripts/release-version.sh", "--print"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
    except subprocess.CalledProcessError as exc:
        fail(f"release-version.sh failed: {exc.stderr.strip()}")


def _validate_release(release: dict, version: str, source_commit: str) -> None:
    if str(release.get("version", "")).lstrip("v") != version:
        fail(f"portal release version {release.get('version')} != {version}")
    if release.get("gitCommit") != source_commit:
        fail(
            f"portal release {version} records source {release.get('gitCommit')}, "
            f"not the immutable tag commit {source_commit}; refusing to publish"
        )
    if release.get("channel") != "stable":
        fail(f"portal release {version} channel is {release.get('channel')}, not stable")
    if release.get("paused"):
        fail(f"portal release {version} is paused")


def cmd_prepare(args: argparse.Namespace) -> None:
    version = _assert_tag_source(args.tag, args.source_commit)
    if args.version and args.version != version:
        fail(f"--version {args.version} != manifest version {version}")
    portal = Portal(_env("PORTAL_URL"))
    portal.login()
    release = portal.release(version)
    if release is None:
        created = portal.create_release(version, args.source_commit)
        release = portal.release(version)
        if release is None:
            fail(f"portal did not return the release {version} after creation")
        print(f"created release {version} (HTTP {'201' if created else '409 idempotent'})")
    else:
        # An existing row is reusable only when it names this exact immutable
        # tag commit. That includes the empty 26.10.3.1 row from the failed run:
        # it is allowed only because its source was already v26.10.3.1's commit.
        print(f"reusing existing release {version}")
    _validate_release(release, version, args.source_commit)
    print(f"prepare: {version} source={args.source_commit} release_id={release['id']}")
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with open(output, "a", encoding="utf-8") as handle:
            handle.write(f"release_id={release['id']}\n")
            handle.write(f"version={version}\n")
            handle.write(f"source_commit={args.source_commit}\n")
            handle.write("publish=true\n")


def _find_asset(release: dict, name: str) -> dict | None:
    assets = release.get("assets") or {}
    if not isinstance(assets, dict):
        fail("portal release assets payload is not an object")
    for asset in assets.get(COMPONENT, []):
        if asset.get("name") == name:
            return asset
    return None


def cmd_upload(args: argparse.Namespace) -> None:
    _env("PORTAL_ADMIN_PASSWORD")
    package = Path(args.package)
    if not package.is_file():
        fail(f"package {package} does not exist")
    if package.stat().st_size <= 0:
        fail(f"package {package} is empty")
    name = package.name
    if name not in EXPECTED_ASSETS:
        fail(f"package {name} is not one of the five official release assets")
    if args.name and args.name != name:
        fail(f"--name {args.name} != package basename {name}")
    digest, size = _sha256_file(package)
    if size <= 0:
        fail("0-byte package cannot be uploaded")
    portal = Portal(_env("PORTAL_URL"))
    portal.login()
    release = portal.release(args.version)
    if release is None:
        fail(f"release {args.version} does not exist; prepare must run first")
    _validate_release(release, args.version, args.source_commit)
    existing = _find_asset(release, name)
    if existing is not None:
        same = (
            str(existing.get("sha256", "")).lower() == digest
            and int(existing.get("sizeBytes", -1)) == size
        )
        if not same:
            fail(
                f"refusing to overwrite {name}: portal has sha256={existing.get('sha256')} "
                f"size={existing.get('sizeBytes')}, local has sha256={digest} size={size}"
            )
        if existing.get("hasFile"):
            print(f"idempotent: {name} already has identical bytes; no upload")
        else:
            print(f"repair: {name} metadata exists but payload is missing; re-uploading")
            existing = None
    if existing is None:
        uploaded = portal.upload_asset(release["id"], name, package)
        if (
            str(uploaded.get("sha256", "")).lower() != digest
            or int(uploaded.get("size_bytes", -1)) != size
        ):
            fail(
                f"portal stored {name} as sha256={uploaded.get('sha256')} "
                f"size={uploaded.get('size_bytes')}, expected {digest}/{size}"
            )
        release = portal.release(args.version)
        if release is None:
            fail(f"release {args.version} disappeared after upload")
        stored = _find_asset(release, name)
        if stored is None or not stored.get("hasFile"):
            fail(f"portal release metadata does not expose {name} with a stored payload")
        if (
            str(stored.get("sha256", "")).lower() != digest
            or int(stored.get("sizeBytes", -1)) != size
        ):
            fail(f"portal metadata for {name} disagrees with the uploaded bytes")
    receipt = {
        "name": name,
        "component": COMPONENT,
        "version": args.version,
        "size": size,
        "sha256": digest,
        "source_commit": args.source_commit,
    }
    receipt_path = Path(args.receipt_dir) / f"{Path(name).stem}.json"
    _write_json_atomic(receipt_path, receipt)
    print(f"upload: {name} sha256={digest} size={size} receipt={receipt_path}")


def _load_receipts(directory: Path, version: str, source_commit: str) -> dict[str, dict]:
    if not directory.is_dir():
        fail(f"receipt directory {directory} does not exist")
    files = sorted(directory.rglob("*.json"))
    if not files:
        fail("0 receipts found; refusing to promote an empty release")
    receipts: dict[str, dict] = {}
    for path in files:
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            fail(f"receipt {path} is not valid JSON: {exc}")
        if not isinstance(payload, dict) or set(payload) != RECEIPT_KEYS:
            fail(f"receipt {path} must contain exactly {sorted(RECEIPT_KEYS)}")
        name = payload.get("name")
        if not isinstance(name, str) or name not in EXPECTED_ASSETS:
            fail(f"receipt {path} has unknown asset name {name!r}")
        if name in receipts:
            fail(f"duplicate receipt for {name}; refusing promotion")
        if payload.get("component") != COMPONENT:
            fail(f"receipt {path} component {payload.get('component')} != {COMPONENT}")
        if payload.get("version") != version:
            fail(f"receipt {path} version {payload.get('version')} != {version}")
        if payload.get("source_commit") != source_commit:
            fail(
                f"receipt {path} source_commit {payload.get('source_commit')} "
                f"!= {source_commit}"
            )
        size = payload.get("size")
        if isinstance(size, bool) or not isinstance(size, int) or size <= 0:
            fail(f"receipt {path} size is not a positive integer")
        sha = payload.get("sha256")
        if not isinstance(sha, str) or not SHA256.fullmatch(sha.lower()):
            fail(f"receipt {path} sha256 is not 64 hex characters")
        receipts[name] = payload
    if set(receipts) != set(EXPECTED_ASSETS):
        missing = sorted(set(EXPECTED_ASSETS) - set(receipts))
        extra = sorted(set(receipts) - set(EXPECTED_ASSETS))
        fail(
            f"expected exactly 5 unique receipts, found {len(receipts)} "
            f"(missing={missing}, extra={extra}); refusing promotion"
        )
    return receipts


def _download_key(portal: Portal, version: str) -> tuple[str, str | None]:
    supplied = os.environ.get("PORTAL_UPDATE_KEY", "")
    if supplied:
        _mask(supplied)
        return supplied, None
    name = f"ci-promote-{version}-{os.environ.get('GITHUB_RUN_ID', 'local')}"
    key_id, key = portal.create_update_key(name)
    return key, key_id


def cmd_promote(args: argparse.Namespace) -> None:
    _env("PORTAL_ADMIN_PASSWORD")
    receipts = _load_receipts(Path(args.receipts), args.version, args.source_commit)
    portal = Portal(_env("PORTAL_URL"))
    portal.login()
    release = portal.release(args.version)
    if release is None:
        fail(f"release {args.version} does not exist")
    _validate_release(release, args.version, args.source_commit)
    assets = (release.get("assets") or {}).get(COMPONENT)
    if not isinstance(assets, list):
        fail(f"release {args.version} has no {COMPONENT} assets")
    server_names = {str(asset.get("name")) for asset in assets}
    if server_names != set(EXPECTED_ASSETS):
        fail(
            f"release {args.version} asset set is not exactly the five official packages: "
            f"{sorted(server_names)}"
        )
    for name in EXPECTED_ASSETS:
        asset = next(a for a in assets if a.get("name") == name)
        receipt = receipts[name]
        if not asset.get("hasFile"):
            fail(f"{name} has metadata but no uploaded payload")
        if str(asset.get("sha256", "")).lower() != receipt["sha256"].lower():
            fail(
                f"{name} portal sha256 {asset.get('sha256')} != receipt "
                f"{receipt['sha256']}"
            )
        if int(asset.get("sizeBytes", -1)) != receipt["size"]:
            fail(
                f"{name} portal size {asset.get('sizeBytes')} != receipt {receipt['size']}"
            )

    key, ephemeral_id = _download_key(portal, args.version)
    try:
        with tempfile.TemporaryDirectory(prefix="portal-verify-") as tmp:
            for name in EXPECTED_ASSETS:
                destination = Path(tmp) / name
                actual = portal.download(args.version, name, key, destination)
                expected_sha = receipts[name]["sha256"].lower()
                if actual.lower() != expected_sha:
                    fail(
                        f"authenticated download of {name} has sha256 {actual}, "
                        f"receipt says {expected_sha}; refusing to promote"
                    )
                size = destination.stat().st_size
                if size != receipts[name]["size"]:
                    fail(
                        f"authenticated download of {name} has size {size}, "
                        f"receipt says {receipts[name]['size']}; refusing to promote"
                    )
                print(f"verified download: {name} sha256={actual} size={size}")
    finally:
        if ephemeral_id:
            try:
                portal.delete_update_key(ephemeral_id)
            except Exception as exc:  # cleanup must not mask the real verdict
                print(f"warning: could not delete ephemeral update key: {exc}", file=sys.stderr)

    portal.set_latest(release["id"])
    verified = portal.release(args.version)
    if verified is None or not verified.get("is_latest_stable"):
        fail(f"set-latest returned but {args.version} is not is_latest_stable")
    print(f"promoted {args.version} to stable latest (source={args.source_commit})")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    prepare = sub.add_parser("prepare", help="validate tag source and create/resume release")
    prepare.add_argument("--tag", required=True)
    prepare.add_argument("--source-commit", required=True)
    prepare.add_argument("--version", default="")
    prepare.set_defaults(func=cmd_prepare)

    upload = sub.add_parser("upload", help="upload one package and write a receipt")
    upload.add_argument("--package", required=True)
    upload.add_argument("--version", required=True)
    upload.add_argument("--source-commit", required=True)
    upload.add_argument("--receipt-dir", required=True)
    upload.add_argument("--name", default="")
    upload.set_defaults(func=cmd_upload)

    promote = sub.add_parser("promote", help="verify five receipts and set latest stable")
    promote.add_argument("--receipts", required=True)
    promote.add_argument("--version", required=True)
    promote.add_argument("--source-commit", required=True)
    promote.set_defaults(func=cmd_promote)
    return parser


def main() -> None:
    args = build_parser().parse_args()
    try:
        args.func(args)
    except PortalError as exc:
        fail(str(exc))
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 - every unexpected failure must be red
        fail(f"unexpected failure: {exc}")


if __name__ == "__main__":
    main()
