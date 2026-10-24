"""Real TLS / HTTP/2 / SNI / payload verification.

This uses the system ``curl`` and ``openssl`` binaries, both present on the
Ubuntu runner images, and fails closed:

* ``http_version`` must be exactly ``2`` (a negotiated HTTP/1.1 fallback is a
  failure, not a warning).
* ``ssl_verify_result`` must be ``0`` and the CA file must be supplied.
* ``openssl s_client -servername`` must report a verified peer, SNI must match
  the requested host, and ALPN must negotiate ``h2``.
* The response body digest must equal the request payload digest.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import tempfile
from dataclasses import dataclass


class TlsVerifyError(RuntimeError):
    """The real TLS/HTTP/2/SNI verification did not meet the contract."""


@dataclass(frozen=True)
class TlsVerifyResult:
    http_version: str
    ssl_verify_result: int
    http_code: int
    body_sha256: str
    alpn: str
    sni: str
    peer_verify: str
    payload_bytes: int

    def to_dict(self) -> dict:
        return self.__dict__.copy()


def _binary(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise TlsVerifyError(f"required binary {name!r} is not installed")
    return path


def curl_supports_http2() -> bool:
    result = subprocess.run(
        [_binary("curl"), "--version"], capture_output=True, text=True, timeout=10
    )
    if result.returncode != 0:
        raise TlsVerifyError(f"curl --version failed: {result.stderr.strip()}")
    # The Features line lists HTTP2 when nghttp2 support is compiled in.
    for line in result.stdout.splitlines():
        if line.startswith("Features:"):
            return "HTTP2" in line.split()
    return False


def parse_curl_writeout(text: str) -> dict:
    parts = text.strip().split("\t")
    if len(parts) != 4:
        raise TlsVerifyError(f"unexpected curl write-out: {text!r}")
    version, verify, code, size = parts
    try:
        verify_int = int(verify)
        code_int = int(code)
        size_int = int(size)
    except ValueError as exc:
        raise TlsVerifyError(f"non-numeric curl write-out: {text!r}") from exc
    return {
        "http_version": version,
        "ssl_verify_result": verify_int,
        "http_code": code_int,
        "size_download": size_int,
    }


def _sha256_path(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify(
    *,
    host: str,
    port: int,
    ca_file: str,
    path: str = "/issue34/payload",
    payload: bytes | None = None,
    timeout: float = 20.0,
) -> TlsVerifyResult:
    if not host:
        raise TlsVerifyError("SNI host is required")
    if not os.path.isfile(ca_file):
        raise TlsVerifyError(f"CA file does not exist: {ca_file}")
    if not curl_supports_http2():
        raise TlsVerifyError("curl was built without HTTP/2 support; cannot verify h2")
    payload = payload if payload is not None else (b"issue34-payload\n" * 4096)
    if not payload:
        raise TlsVerifyError("payload must not be empty")

    with tempfile.TemporaryDirectory(prefix="issue34-tls-") as tmp:
        request_path = os.path.join(tmp, "request.bin")
        body_path = os.path.join(tmp, "body.bin")
        with open(request_path, "wb") as handle:
            handle.write(payload)
        url = f"https://{host}:{port}{path}"
        command = [
            _binary("curl"),
            "--silent",
            "--show-error",
            "--fail-with-body",
            "--http2",
            "--cacert",
            ca_file,
            "--resolve",
            f"{host}:{port}:127.0.0.1",
            "--request",
            "POST",
            "--data-binary",
            f"@{request_path}",
            "--output",
            body_path,
            "--write-out",
            "%{http_version}\t%{ssl_verify_result}\t%{http_code}\t%{size_download}",
            "--max-time",
            str(timeout),
            url,
        ]
        result = subprocess.run(command, capture_output=True, text=True, timeout=timeout + 5)
        if result.returncode != 0:
            raise TlsVerifyError(
                f"curl HTTP/2 request failed (exit {result.returncode}): {result.stderr.strip()}"
            )
        parsed = parse_curl_writeout(result.stdout)
        if parsed["http_version"] != "2":
            raise TlsVerifyError(
                f"HTTP/2 fallback detected: negotiated {parsed['http_version']!r}"
            )
        if parsed["ssl_verify_result"] != 0:
            raise TlsVerifyError(
                f"TLS verification failed: ssl_verify_result={parsed['ssl_verify_result']}"
            )
        if parsed["http_code"] != 200:
            raise TlsVerifyError(f"unexpected HTTP status {parsed['http_code']}")
        body_sha = _sha256_path(body_path)
        expected_sha = hashlib.sha256(payload).hexdigest()
        if body_sha != expected_sha:
            raise TlsVerifyError(
                f"payload digest mismatch: sent {expected_sha}, received {body_sha}"
            )

        sclient = subprocess.run(
            [
                _binary("openssl"),
                "s_client",
                "-connect",
                f"127.0.0.1:{port}",
                "-servername",
                host,
                "-CAfile",
                ca_file,
                "-verify_return_error",
                "-verify_hostname",
                host,
                "-alpn",
                "h2",
            ],
            input="",
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        output = sclient.stdout + sclient.stderr
        if sclient.returncode != 0:
            raise TlsVerifyError(
                f"openssl s_client failed (exit {sclient.returncode}): {output.strip()[-2000:]}"
            )
        verified = "Verify return code: 0 (ok)" in output or "Verification: OK" in output
        if not verified:
            raise TlsVerifyError(
                "openssl s_client did not verify the peer certificate: "
                + output.strip()[-2000:]
            )
        if "ALPN protocol: h2" not in output:
            raise TlsVerifyError(
                "openssl s_client did not negotiate ALPN h2: " + output.strip()[-2000:]
            )
        if "subject=" not in output:
            raise TlsVerifyError("openssl s_client did not report a peer subject")

    return TlsVerifyResult(
        http_version=parsed["http_version"],
        ssl_verify_result=parsed["ssl_verify_result"],
        http_code=parsed["http_code"],
        body_sha256=body_sha,
        alpn="h2",
        sni=host,
        peer_verify="ok",
        payload_bytes=len(payload),
    )
