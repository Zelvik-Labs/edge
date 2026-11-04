# OFF-only framework control

Build the standalone control from the same locked Pingora/Rustls/mimalloc sources,
then run Edge/bare Pingora/Nginx/OpenResty independently in balanced order.
48 cells: H1/H1-TLS/H2 OFF/reuse, 1 KiB, concurrency32, four rounds.

The immutable Edge binary is ba474; it is not a current-head throughput claim.
The bare control contains no Edge business library and is not a feature-equivalent replacement.
Keep all failures, compressed and uncompressed records, wrong-CA controls and origin connection observations.
No vendor edits or local Rust compilation are required; GitHub Actions performs the build and run.

Method, scope and results: [architecture report](../../../docs/performance-bare-pingora-2026-11-04.md).
