# UDP demux configuration lookup

Dispatch CI with `udp_demux_benchmark=true`; add `xdp_node_smoke=true` for the
independent full-node protocol/reload checks. The component builds first, runs on
one CPU, and uploads build messages, hashes, profile, logs, CPU snapshots and raw
Criterion samples, including partial output on failure.

The same binary compares the production listener-set APIs, retained pre-index
snapshot scan, and indexed point query for 1/1,000 H3 sites plus one QUIC
forwarding site. It checks global 8443 and per-site port policies, H3/QUIC
matches and ordinary UDP misses: 48 lookup functions. Another four functions
measure construction of the actual production bitmap helper for 1/1,000
repeated narrow/full-u16 ranges, including allocation and drop. These construction
functions exclude parsing and other configuration update work. Each function
has 30 samples. The runner rejects incomplete output and
recomputes per-iteration means from the retained `iters` and `times` arrays.

These are sequential configuration lookups using the System allocator. The set
control also uses the new ASCII suffix predicate. Results neither isolate the
suffix change nor measure historical old binaries, network throughput, H3
capacity or Nginx/OpenResty capacity. The retained scan method has the original
point-query body and is used solely as a measurement control; serving uses the
index without a runtime mode branch. Site membership is an immutable bitmap,
published under the existing configuration lock on every server update/removal.
The selected global policy and QUIC forwarding maps are read in that same epoch.
An empty index allocates no bitmap; a populated index uses 8 KiB of bitmap data
plus its Arc allocation. Full NodeConfig clones share that immutable data.
No TTL or invalidation timer is involved. Range insertion touches at most 1,024
words per configured range instead of expanding a full range into 65,536 ports.

Locally, follow the workspace mbx/storage rules. The Rust wrapper supports
`build --bench udp_demux_bench`, not the `bench` subcommand. Keep its complete raw
output; pass only Cargo JSON messages to `run.py --build-messages … --output …`.
Record the actual profile: local development builds are not release measurements.
The output directory must be new. Finish all compilation before running; the
runner rejects active compilers. No benchmark is run against production traffic.
