# Idle queue cooperation validation

This independent crate imports the production queue helper directly from
`src/xdp/af_xdp/cooperation.rs`, including its tests. It does not copy the helper
or replace the full-node integration tests. Tokio is pinned to the node's
version. The ordinary queue mode remains eager; the candidate is selected only
by `xdp proxy-smoke` / `proxy-reload-smoke --idle-yield cooperative`.

The current-thread test executes a finite number of immediately ready idle
rounds and requires a separate spawned task to progress. Removing the idle
cooperative budget must fail this test. A separate manual first-poll test
requires every busy round to remain pending, preserving the existing explicit
yield contract. Replacing that yield with a cooperative budget must fail it.
These tests do not claim strict timer deadlines under loaded networking.

From the repository root, CI runs:

```bash
python3 scripts/perf/idle-yield/validate.py --output .local/idle-yield-ci/results
```

The driver tests production, each intentional fault, and restored production,
then runs Clippy. Each fault must compile and fail exactly its corresponding
test, while the other test passes. It restores the exact source bytes in a
`finally` block and checks the source digest before the final production run.
Logs and statuses remain inspectable, including a failed validation. Local Rust
commands must use the workspace build wrapper and shared target; this driver is
for the isolated GitHub runner. All compilation ends before network timing.

The CI inputs `xdp_node_smoke=true`, `xdp_udp_benchmark=true` and
`xdp_udp_comparison=idle-yield` run the complete functional matrix and matched
one/two-CPU UDP comparisons with pinned Nginx/OpenResty. Candidate gains must
come from those uninstrumented measurements; helper test duration is not a
performance result. The experiment does not measure HTTP cache throughput.
