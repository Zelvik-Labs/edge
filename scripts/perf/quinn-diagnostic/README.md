# Native fresh-connection protocol diagnosis

Reuse the exact Edge/controller from GSO run 41714149785, artifact 13271188885.
Both archive and actual binary hashes are pinned and verified. Runtime source
is `82e3cf0db722497fb594acdcfabc645a6deb2b56`; no Rust compilation occurs here.

The client has an explicit `EDGE_QUIC_DIAGNOSTICS` build. The normal client
does not enable it. OpenSSL's public SSL_CTX/message callbacks observe QUIC
control frames and stream/CRYPTO headers; DATAGRAM payloads, packet payloads,
TLS secrets and HTTP body data are omitted. Each active slot retains at most
64 KiB, and only failed transfers print the recent trace (up to the existing
64-error cap per shard). Truncation and dropped bytes remain observable.
Close frames decode transport/application code, triggering frame and up to
256 reason bytes; the full reason length and hex prefix are preserved.

Native/shared/shared/native order runs four independent 32KiB HIT cases,
16,384 fresh attempts each, same matched settings, Retry off and GSO auto.
The verifier proves the context and QUIC-frame callbacks actually ran, checks
actual executables and all transfer records, and retains missing diagnoses.
Errors still fail the workflow. Instrumented RPS/P99 are excluded from capacity
comparisons. A clean run or absent close frame does not establish a root cause.

ASan/UBSan controls cover transport/application close frames, truncation,
62-bit varints, bounded trace memory and the public context callback.
The public collector keeps only explicit evidence names, including the client
sources; TLS keys, overrides, node logs and stores stay private.

```sh
python3 scripts/perf/quinn-diagnostic/verify.py /path/to/evidence \
  /path/to/extracted/quinn-diagnostic-binaries
```
