# SNI ClientHello parser comparison

The production parser is shared by kernel peek and AF_XDP prefix replay. This
probe extracts the exact old function and enum from immutable `BASE_REF`, the
first bounded-allocation repair from `BOUNDED_REF`, direct record reader from
`CURSOR_REF`, and current production module. Only original baseline visibility is adjusted so the same probe can
call it. No old upstream or vendor source is changed.

GitHub Actions builds both variants with Rust 1.99.1, x86-64-v2 and opt-level 3.
Timing uses the normal System allocator. Separate executables count allocation
calls, requested bytes and the largest allocation; those counters are not RSS.
All binaries are built before any measurement starts.

The old/new output corpus covers every prefix of five valid/incomplete inputs
and 4096 deterministic mutations, plus every supported small-hello split with
an empty middle record and every one-bit mutation of each such wire input.
A negative allocation control must fail on
the old parser's untrusted 24-bit handshake length and pass on the candidate.
Two four-variant Williams-design cycles are pinned to the same CPU: 32
processes and 3200 blocks, with eight matched comparisons against each prior
implementation. Each variant appears in every position and follows every
other variant once per cycle. Every block and
host steal snapshot is retained. These are parser component timings, not
complete SNI connection or Node throughput measurements.

The standalone workflow needs no private runtime dependencies:

```sh
python3 scripts/perf/sni-client-hello/run.py build
python3 scripts/perf/sni-client-hello/run.py measure
python3 scripts/perf/sni-client-hello/run.py verify
```

Use the managed workspace Rust wrapper for any local Rust builds; the commands
above are the CI workflow. `check-source` performs extraction checks without
compiling. Root CI separately validates integration, existing SNI routing and
prefix-replay tests, and the AF_XDP protocol/reload smoke matrix.
