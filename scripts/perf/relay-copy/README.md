# Generic relay copy accounting comparison

The builder extracts the exact old direction function from immutable commit
`b4d635519973b2ec9aa24a533129fdbb86d3fc4f`, the candidate function and its
shared progress guard, and both versions' error-classification functions.
Original files, extracted source, lockfiles and actual executables are retained.

This is a **component fixture**, not a complete TCP/SNI or AF_XDP node:

- The copy buffer is fixed at 64 KiB and pressure policy is disabled, as it is
  for long-lived SNI passthrough. The options fixture contains the one field
  read by this function. Actual memory-governor and metric-map lookups are not
  measured; the metric callback accumulates into one atomic counter.
- Input/output are ready in-memory async implementations. The output actually
  copies bytes and is checked. Full writes and 64-byte short writes are separate.
- Each one-direction invocation uses the production counter ownership pattern:
  one heap Arc plus clone in the baseline, a borrowed stack atomic in the
  candidate. This counts one of the two directions in a complete relay.
- The old control must fail exact accounting after an accepted partial write.
  Separate controls retain partial error, WriteZero, cancellation during a
  write, and cancellation while awaiting the next read. Error classification
  must match; an accounting repair cannot turn WriteZero into success.
- Timing uses the normal System allocator. Allocation counts use separate
  executables. Five alternating process pairs share one pinned CPU; all 800
  timing blocks and CPU steal snapshots are retained, with no outlier removal.

GitHub Actions builds both variants completely before measurement. Root CI
separately exercises the real metric registry, pressure/half-close behavior,
protocol integration and the native AF_XDP matrix.

```sh
python3 scripts/perf/relay-copy/run.py build
python3 scripts/perf/relay-copy/run.py measure
python3 scripts/perf/relay-copy/run.py verify --directory /path/to/unpacked/artifact
```

The first two are CI commands; local Rust builds must use the managed workspace
wrapper and meet its disk floor. `check-source` does not compile Rust.
