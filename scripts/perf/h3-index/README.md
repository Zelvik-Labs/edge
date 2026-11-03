# Site-listener bitmap gate

This dependency-free crate compiles the actual `src/config/h3_ports.rs` tests.
Its full-u16 differential checks cover empty, narrow, overlapping, full-domain
and word-boundary ranges. The clone test verifies that extending a new snapshot
does not change the old snapshot's membership.

CI invokes `validate.py --output .local/h3-index-ci/results` when
`udp_demux_benchmark=true`. It tests production, deliberately zeroes the middle
words of inserted ranges, requires exactly the reference-union test to fail,
restores exact source bytes, tests production again and runs Clippy. Logs,
statuses and source hashes are uploaded even when a gate fails. Local Rust
validation must use the workspace wrapper and shared target; this driver runs
on the isolated CI runner.

This gate covers the bitmap only. Root tests separately compare 37 parser
fixtures against the frozen pre-change parser across every u16 port, exercise
the configuration update/removal APIs and concurrent snapshot publication, and
retain the full H3/QUIC listener-set differential. Full-node protocol/reload
checks remain required. The 52-function Criterion component reports lookup and
bitmap construction costs separately, without claiming whole-node throughput.
