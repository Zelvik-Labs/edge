# Scoped WAF table reservation probe

Compile the actual, unchanged `src/firewall/bounded_map.rs` in an isolated crate.
The production map's seven unit tests run first, followed by Clippy and release
compilation. Only after all compilers exit, measure seven tables in fresh
processes at requested capacities 65,536, 1,677,721 and 8,000,000, five rounds
each with alternating size order. Capacities are representative choices, not
the full node's resolved governor value.

Retain construction time, actual Rust slot size/alignment, physical slot count,
live empty RSS before/after, first insert times, empty-table visit/retain scans
and 200,000 verified hit/miss lookups. Raw logs, CPU snapshots, process CPU,
memory limits, affinity, compiler/binary/source hashes and all samples remain.
The subprocess uses the System allocator; Edge uses mimalloc. This identifies
component reservation costs, not exact whole-node allocation attribution,
network throughput or a proven improvement. No serving code changes here.

Local Rust compilation must use the repository workspace wrapper and its free
space floor. The branch-scoped workflow measures on an independent cloud runner;
it does not share or change the concurrent UDP benchmark's runtime or results.

Before any lazy-allocation candidate, measure first touch as well as steady
operations, preserve the eight-slot probe and eviction order, and audit global
soft-cap concurrency and corner-path segment locking. Do not reduce capacity,
silently skip rules or substitute absent measurements with zeros.
