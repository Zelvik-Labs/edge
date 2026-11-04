# QUIC periodic maintenance full CI

Run 41761049818, source e640a76caee4d7dcc12e77d8b2fc0cefbd753a7e.
1363 library +16 binary tests; integration/vendor/check/Clippy passed.
32 native functional combinations passed; no throughput claim.

Artifact 13292272103 has 69 files. DOWNLOAD must contain its raw.zip,
artifact.json, manifest.json, plus the test job log as job.log and the frozen
test-summary.json from this directory.

```sh
python3 docs/evidence/quic-maintenance-2026-11-04/verify.py DOWNLOAD --commit e640a76caee4d7dcc12e77d8b2fc0cefbd753a7e
```

The log and every ZIP entry are hashed; all case exits, completion markers,
network cleanup and binary identity metadata are checked. Actual node
executables are not in this artifact and are not rehashed by this verifier.
