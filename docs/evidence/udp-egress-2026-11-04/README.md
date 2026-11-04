# Downstream component tradeoffs

Both candidates remain outside production. Reserve-first improves rejection
cost but ready64/1200 regress5.12/7.42%. Advisory snapshots reduce those costs
to2.15/1.68%, with a concurrent-full race still checked by original try_send.

```sh
python3 docs/evidence/udp-egress-2026-11-04/verify.py reserve-first FIRST_DOWNLOAD
python3 docs/evidence/udp-egress-2026-11-04/verify.py snapshot SECOND_DOWNLOAD
```

Each download keeps raw.zip/artifact.json/manifest.json/extracted. Artifacts
13292844479 and13294045455 include actual binaries, separate allocation and
timing builds, exact helpers,12controls/variant and3840blocks/run.
The verifier recomputes all records; these are not node/XDP capacity results.
