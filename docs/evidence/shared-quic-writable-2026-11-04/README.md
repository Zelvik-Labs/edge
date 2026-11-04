# Shared QUIC writable full CI

Source c310473, run41765823252:1366library+16binary tests,
integration/vendor/check/Clippy and32native combinations passed.
This is readiness/correctness evidence, not a capacity comparison.

Download artifact13294298409 to raw.zip/artifact.json/manifest.json and
retain the exact test job log as job.log plus the frozen test-summary.json.

```sh
python3 docs/evidence/shared-quic-writable-2026-11-04/verify.py DOWNLOAD --commit c31047359d14ec1a2596d244be8690960b49d7f8
```

All69archive files, test log hash, source, completion markers and case
cleanup are checked. This artifact does not contain the actual node
executables; binary identity metadata is not an executable rehash.
