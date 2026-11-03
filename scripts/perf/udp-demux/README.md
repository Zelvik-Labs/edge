# UDP demux configuration lookup

Dispatch CI with `udp_demux_benchmark=true`; add `xdp_node_smoke=true` for the
independent full-node protocol/reload checks. The component builds first, runs on
one CPU, and uploads build messages, hashes, profile, logs, CPU snapshots and raw
Criterion samples, including partial output on failure.

The same binary compares the production listener-set APIs with the snapshot
point query for 1/1,000 H3 sites plus one QUIC forwarding site. It checks global
8443 and per-site port policies, H3/QUIC matches and ordinary UDP misses: 32
functions, each with 30 samples. The runner rejects incomplete output and
recomputes per-iteration means from the retained `iters` and `times` arrays.

These are sequential configuration lookups using the System allocator. The set
control also uses the new ASCII suffix predicate. Results neither isolate the
suffix change nor measure historical old binaries, network throughput, H3
capacity or Nginx/OpenResty capacity. Per-site fallback still scans sites under a
read lock; no derived index or TTL cache is installed.

Locally, follow the workspace mbx/storage rules. The Rust wrapper supports
`build --bench udp_demux_bench`, not the `bench` subcommand. Keep its complete raw
output; pass only Cargo JSON messages to `run.py --build-messages … --output …`.
Record the actual profile: local development builds are not release measurements.
The output directory must be new. Finish all compilation before running; the
runner rejects active compilers. No benchmark is run against production traffic.
