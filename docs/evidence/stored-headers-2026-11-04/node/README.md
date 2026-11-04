# Stored-header node comparison

Run 41757321970; only src/cache/mod.rs differs in the two production builds.
192 passed cells, 6,854,656 successful responses, 128 passed Range controls.
Public files are an explicit allow-list, without private fixture TLS or overrides.
Download evidence artifact 13291166935 and binary artifact 13290468969 into the
standard raw.zip/artifact.json/manifest.json/extracted layout, then run:

```sh
python3 docs/evidence/stored-headers-2026-11-04/node/verify.py EVIDENCE BINARIES
```

The frozen verifier checks both archives, all actual binaries, every transfer
and paired metrics, comparing the recomputed records and summary to this copy.
Four short rounds do not establish statistical significance for small changes.
