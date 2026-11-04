# Persisted header validation candidate

This experiment uses the exact cache helpers from commit
`a79b20d22c32e2b77c926e2335792b874b1d8bc8` and a streaming candidate in
`benches/support/stored_headers_candidate.rs`. The first measured candidate is now integrated into production; CI extracts its
exact implementation and checks it against the frozen candidate before building.
The candidate retains `http` HeaderName/HeaderValue validation and all shared
cache privacy, duplicate length and stored-header gates. Up to 32 fields avoid
a temporary HeaderMap; larger inputs use the exact original implementation.
That threshold chooses an optimization and does not impose a new rejection.

The standalone lockfile must exactly match production http/bytes/itoa versions
and checksums. Actions builds timing and allocation executables separately,
then checks more than 30,000 inputs including mixed case, repeated lengths,
all ASCII bytes, UTF-8, privacy/framing directives, long names and the 32/33
field boundary. Every decision is retained, not just an aggregate assertion.

Eight alternating pairs of pinned processes cover eight fixtures with twenty
10ms blocks each. Allocator counters are absent from the timing executable.
Actual binaries, extracted helpers, original file, build logs, allocations,
raw timing blocks and host CPU snapshots are retained. This is a component
experiment; it does not establish production throughput or eliminate all
header allocations. Both variants still use the real HTTP parsers.

```sh
python3 scripts/perf/stored-headers/run.py verify --directory /path/to/artifact
```
