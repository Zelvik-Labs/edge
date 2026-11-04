# UDP downstream candidate

Reserve bounded queue capacity before constructing a borrowed payload copy.
Keep the exact observable Full/Closed errors, owned Bytes identity, queue
capacity, addresses, timestamp and recovery behavior. This is a candidate,
not production integration or a real AF_XDP ring / node throughput result.

The builder extracts the immutable c310473 channel and real monotonic clock;
the candidate lives under benches/support. Actual source/lockfiles and both
executables are retained. Timing disables allocation counters; allocation
counts run in another executable. Ready cases include queue consumption,
Full/Closed cases keep that state throughout. Errors still allocate their
original error objects; only rejected borrowed payload copies are avoided.

12 cases cover 64/1200/65507-byte borrowed and 1200-byte owned payloads under
Ready/Full/Closed. Eight alternating pairs, 20 blocks/case, pinned CPU, steal
tracking, no overlapping compiler. Controls verify complete payloads,
addresses, owned identity, unchanged queued packets, recovery and closure.

```sh
python3 scripts/perf/udp-egress/run.py build
python3 scripts/perf/udp-egress/run.py measure
python3 scripts/perf/udp-egress/run.py verify --directory DOWNLOADED_EXTRACTED
```

Rust compilation runs in Actions under the workspace budget policy.
