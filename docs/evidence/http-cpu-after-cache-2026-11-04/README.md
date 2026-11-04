# Post-cache CPU attribution

Run41767019693 / harness8e0c433; actual runtime ba474, original client SHA pinned.
18 conditions, 4,961,504 successful responses, 43,074 samples, no errors/loss/steal.
Inclusive groups overlap. Instrumented throughput is excluded from ranking.

```sh
python3 docs/evidence/http-cpu-after-cache-2026-11-04/verify.py EVIDENCE HEADER_BINARIES ORIGINAL_CLIENT
```

EVIDENCE is artifact13293665946; HEADER_BINARIES is artifact13290468969.
Each uses raw.zip/artifact.json/manifest.json/extracted. ORIGINAL_CLIENT is
the actual original protocol-state-client from artifact13250758275 (or an
identical file matching its pinned SHA256). All three executable hashes,
every archive byte, each transfer and each symbolized sample are checked.
Raw sampled memory, private TLS and runtime snapshots are excluded.
