# Full-node XSK wake integration gate

The CI `workflow_dispatch` input `xdp_node_smoke=true` builds one pair of
`Zelvik-Edge` / `h3-probe` binaries, then runs the existing full application
smoke in worker/readable/readable/worker order. Both modes use the same binary.
`--wake readable` is available only on explicit proxy smoke / reload commands;
normal serving still uses the owned poll worker.

Run only in fresh network **and** mount namespaces. The wrapper checks both
parent IDs, privately mounts bpffs and `/run/netns`, brings up loopback, and
lets the existing fixture own its veth/client namespace and throwaway config.
It requires HTTP/2 support instead of letting that protocol be skipped. Full
stdout/stderr, exit codes, namespace cleanup, and before/after binary hashes
are retained, including failed cases. Each case covers IPv4/IPv6 TCP (including
half-close), HTTP/1.1, HTTPS, HTTP/2, SNI, UDP, H3, QUIC passthrough, generation
replacement, raw redirect/bypass, and unmatched kernel traffic. Existing smoke
TCP retries are unchanged: a passing suite does not prove every first attempt
succeeded. This is a correctness gate, not a throughput benchmark or long soak.

```bash
parent_net=$(readlink /proc/self/ns/net)
parent_mount=$(readlink /proc/self/ns/mnt)
sudo unshare --net --mount --propagation private -- \
  bash scripts/perf/afxdp-node/run.sh \
  "$PWD/target" "$PWD/.local/afxdp-node-ci" "$parent_net" "$parent_mount"
```

Use the repository's build wrapper for local Rust compilation. The wrapper above
does not compile anything or touch RKE. veth runs use explicit SKB/copy mode;
physical NIC zero-copy, RX NEED_WAKEUP and production configuration are not
validated by this fixture. Functional suite wall times are recorded only to
diagnose stalls and must not be reported as latency or capacity improvements.

## Matched UDP reuse comparison

`workflow_dispatch` input `xdp_udp_benchmark=true` builds release Edge and pinned,
unmodified Nginx 1.28.0 / OpenResty 1.27.1.2 before starting any measurement.
The separate `build-baselines.sh` verifies the official tarball SHA-256 digests.
The load generator source is `benches/support/udp-bench.c`; its bytes match the
initial comparison's source, previously stored beside this driver.
Both baseline servers have two workers; Edge runs within the same two-CPU
affinity. A C load generator and a C origin each occupy a separate CPU, requiring
four available CPUs. The controller also needs CPU time; its snapshot windows
and the host's per-CPU activity/steal are retained, rather than assumed free.

The matrix is IPv4 UDP, 64/1200-byte request payloads, concurrency 1/16, one
outstanding request per flow. It runs four balanced Williams-order rounds over
worker/readable/Nginx/OpenResty, plus one preceding direct-origin calibration per
matrix cell: 64 proxy cases and four direct cases. Every case has 1000 warmup
round trips per flow, then 10000 measured round trips. The client validates the
entire reply prefix, payload and sequence; a timeout, loss, corrupt response or
wrong peer count fails the case. It never retries UDP requests. Nginx/OpenResty
use `proxy_requests 0; proxy_responses 2147483647; proxy_timeout 60s;`; the
INT32_MAX response sentinel keeps the upstream UDP session open. The origin's
distinct source ports must equal concurrency, independently verifying reuse.

`udp-run.sh` checks fresh network and mount namespaces and creates an isolated
one-queue veth pair. All servers use the same offload settings, applied *after*
Edge's startup tuner. Edge uses explicit SKB/copy mode and must report XSK
redirects and zero queue faults/drops. The external origin's exact byte-valid
reply counts are authoritative; the node's embedded smoke backends are unused.
SIGTERM requests an ordinary diagnostic shutdown after the timed and idle
phases. The 60-second node lifetime is a safety bound, not the timing interval.

Warmup ends at an explicit client gate; CPU/process snapshots precede the GO
marker. A DONE marker captures the end before samples are formatted. All RTT
samples, process-tree CPU/thread/FD counts, idle CPU, host CPU activity/steal,
source and binary digests, commands and failed attempts are uploaded. CPU
snapshots have wider windows than the timed client phase; their overhead is
reported. Quality flags identify short samples, noisy hosts, saturated client
or origin CPU, and proximity to the direct calibration's throughput. Flags do
not remove samples. Four paired rounds describe dispersion, not a confidence
interval. A failed or missing case withholds its whole matrix cell comparison.

Run `test_udp_bench.py` against the compiled C binary with `UDP_BENCH_BINARY`
and `UDP_BENCH_TEST_OUTPUT` set; run `test_udp_summary.py` for analysis checks.
The loopback tests validate the tool, not AF_XDP support or proxy performance.
This comparison does not cover HTTP cache, HTTP/2, H3, hardware zero-copy,
long-running stability or equivalent production feature costs.
