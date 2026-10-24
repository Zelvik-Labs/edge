#!/usr/bin/env python3
"""Generate a throwaway CA and server certificate for the Issue 34 soak.

The soak must prove CA verification, not disable it. Using a generated CA
(with the leaf signed by it) lets ``curl --cacert`` and
``openssl s_client -CAfile`` verify the peer while keeping all key material
inside the CI workspace. Nothing here is written outside ``--out-dir``.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys

DEFAULT_HOSTS = ("localhost", "edge.issue34.test")


class CertError(RuntimeError):
    """Certificate generation failed."""


def _openssl() -> str:
    path = shutil.which("openssl")
    if not path:
        raise CertError("openssl is not installed")
    return path


def _run(command: list[str], *, stdin: str | None = None) -> None:
    result = subprocess.run(command, input=stdin, capture_output=True, text=True)
    if result.returncode != 0:
        raise CertError(
            f"command failed ({result.returncode}): {' '.join(command)}\n"
            f"stdout: {result.stdout[-2000:]}\nstderr: {result.stderr[-2000:]}"
        )


def generate(out_dir: str, hosts=DEFAULT_HOSTS, days: int = 2) -> dict:
    if days <= 0:
        raise CertError("days must be positive")
    if not hosts:
        raise CertError("at least one host is required")
    openssl = _openssl()
    os.makedirs(out_dir, exist_ok=True)
    ca_key = os.path.join(out_dir, "ca.key")
    ca_pem = os.path.join(out_dir, "ca.pem")
    leaf_key = os.path.join(out_dir, "server.key")
    leaf_csr = os.path.join(out_dir, "server.csr")
    leaf_pem = os.path.join(out_dir, "server.pem")
    ext = os.path.join(out_dir, "server.ext")
    serial = os.path.join(out_dir, "ca.srl")

    _run(
        [
            openssl,
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-days",
            str(days),
            "-nodes",
            "-subj",
            "/CN=Zelvik Issue34 Test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-keyout",
            ca_key,
            "-out",
            ca_pem,
        ]
    )
    _run(
        [
            openssl,
            "req",
            "-newkey",
            "rsa:2048",
            "-sha256",
            "-nodes",
            "-subj",
            "/CN=localhost",
            "-keyout",
            leaf_key,
            "-out",
            leaf_csr,
        ]
    )
    san = ",".join(f"DNS:{host}" for host in hosts) + ",IP:127.0.0.1"
    with open(ext, "w", encoding="ascii") as handle:
        handle.write(
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth\n"
            f"subjectAltName={san}\n"
        )
    _run(
        [
            openssl,
            "x509",
            "-req",
            "-in",
            leaf_csr,
            "-CA",
            ca_pem,
            "-CAkey",
            ca_key,
            "-CAcreateserial",
            "-CAserial",
            serial,
            "-days",
            str(days),
            "-sha256",
            "-extfile",
            ext,
            "-out",
            leaf_pem,
        ]
    )
    os.chmod(ca_key, 0o600)
    os.chmod(leaf_key, 0o600)
    _run(
        [
            openssl,
            "verify",
            "-CAfile",
            ca_pem,
            "-verify_hostname",
            hosts[0],
            leaf_pem,
        ]
    )
    manifest = {
        "ca": ca_pem,
        "ca_key": ca_key,
        "cert": leaf_pem,
        "key": leaf_key,
        "sni": hosts[0],
        "hosts": list(hosts),
    }
    with open(os.path.join(out_dir, "manifest.json"), "w", encoding="utf-8") as handle:
        json.dump(manifest, handle, indent=2)
    return manifest


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--host", action="append", default=None)
    parser.add_argument("--days", type=int, default=2)
    args = parser.parse_args(argv)
    try:
        manifest = generate(args.out_dir, args.host or DEFAULT_HOSTS, args.days)
    except CertError as exc:
        print(f"issue34 certs: {exc}", file=sys.stderr)
        return 1
    print(json.dumps(manifest, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
