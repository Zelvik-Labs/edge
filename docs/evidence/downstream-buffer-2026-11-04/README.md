# Downstream buffer node comparison

Run [41824926995](https://github.com/Zelvik-Labs/edge/actions/runs/41824926995)
uses harness source `ebd34770a817b68bd833f876918fc660a0089f49`.
The production comparison is exactly
`4613a6b8d468be8df646f7cb628a42b89bfe0e98` versus
`60cf30ced5f85f68b84f27e170972ca528115434`.
Only three production paths differ; all four Edge/controller executables are
built and retained before any measurements. The release version metadata is
prepared separately; these are measured candidate executables, not the final
tagged release packages.

All 384 cells passed: 7,240,745 successful responses, zero transfer errors,
256 successful Range controls and 48 required wrong-CA rejections. Independent
replay regenerated records and summary exactly, including all 26 negative paired
throughput rounds. Results are in `pass/verified-summary.json` and
`pass/verified-records.json`; `pass/verification.log` records the replay.

Original artifacts:

| Kind | Artifact ID | ZIP bytes | SHA-256 |
|---|---:|---:|---|
| Transfers and controls | 13324504327 | 54,872,983 | `5f6f7e146eb4828019539b8ffa0d9c27fb433998d43aaa2346f2b20db7d85ac1` |
| Measured executables | 13326700281 | 38,784,724 | `7e8f3e7acb9b42464ca6a2d51f62d5c6dc30cd4eb22b45effe862f64bab1f736` |

Actual Edge ELF SHA-256 is `850b9d2ae3bf739caac470b1088e19d33960d3c7b4308fdb5c7554f1072a2f61`
before and `eb7033afaea3f90b91bbfd153cab687582b8e0722ddd4674d7696ce71d827a52`
after. Both actual controller executables have the same SHA-256
`1a85adaec923f45c490b2bcc949adb35c562241e01024b3020aae1c99486b6b2`.

Original, unmodified ZIPs are version attachments:

- [`evidence-downstream-buffer-41824926995.zip`](https://github.com/Zelvik-Labs/edge/releases/download/v26.11.1.1/evidence-downstream-buffer-41824926995.zip)
- [`evidence-downstream-buffer-binaries-41824926995.zip`](https://github.com/Zelvik-Labs/edge/releases/download/v26.11.1.1/evidence-downstream-buffer-binaries-41824926995.zip)

`pass/` and `binaries/` retain each artifact's API digest and per-file extraction
manifest. Save each downloaded ZIP as `raw.zip` beside the matching metadata in
a separate work directory, and extract it to `extracted/`. With full repository
history including `perf/program-wide-20261104`, run:

```bash
python3 docs/evidence/downstream-buffer-2026-11-04/pass/replay.py DATA_DOWNLOAD BINARY_DOWNLOAD
```

The replay verifies both original ZIPs and every extracted byte, actual measured
Edge/controller/client executables, immutable source hashes and the three-path
production difference. It uses the original frozen verifier to recompute all
transfer records, HTTP/cache/connection contracts, Range and wrong-CA controls,
then requires exact equality with the original records and summary. It never
executes a measured binary, substitutes later measurements, or drops negative
rounds. See the [result report](../../performance-downstream-buffer-2026-11-04.md).
