# Full-node XSK wake integration gate

The CI `workflow_dispatch` input `xdp_node_smoke=true` builds one pair of
`Zelvik-Edge` / `h3-probe` binaries, then runs the existing full application
smoke for all eight combinations of worker/readable, full/bridge RX statistics
and eager/cooperative idle yielding, twice each in a forward/reverse order
within each idle mode. All 16 cases use the same binary. This functional order
is distinct from the balanced performance schedule below.
`--wake readable` is available only on explicit proxy smoke / reload commands;
normal serving still uses the owned poll worker.
`--rx-stats full|bridge` is likewise validation-only: normal bridge polling
uses typed RX ownership/readiness observations, while raw diagnostics retain
complete parsed/error statistics. Full mode measures the legacy extra
classification pass inside the shared RX implementation, not byte-identical
historical machine code.
`--idle-yield eager|cooperative` is validation-only too. Normal serving retains
eager per-round yielding. Cooperative mode spends a Tokio cooperative token
after the idle event select completes, even if that select was immediately
ready; busy rounds still explicitly yield every time. No readiness, ring,
pressure, timer or error handling is removed. The helper's standalone tests and
two deliberate scheduling faults are described in `../idle-yield/README.md`.

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

Wire controls send corrupt IPv4 headers, corrupt IPv4/IPv6 UDP checksums and
forbidden zero IPv6 UDP checksums. Raw polling must count all four as parse
errors. Application checks first require valid replies (including legal zero
IPv4 UDP checksums), then require no corrupt replies and exact UDP backend
counts: five valid requests in the default dual-stack fixture. The expected
negative reply timeout is one second; other socket errors fail the case.
`test_corrupt_udp.py` independently checks the generated wire controls.

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
CI runs independent one- and two-proxy-CPU matrices. Baseline worker counts
match each CPU budget; Edge runs within the same affinity and explicitly pins
its single queue to the first selected proxy CPU. A C load generator and a C
origin each occupy a separate CPU, requiring
four available CPUs. The controller also needs CPU time; its snapshot windows
and the host's per-CPU activity/steal are retained, rather than assumed free.

The matrix is IPv4 UDP, 64/1200-byte request payloads, concurrency 1/16, one
outstanding request per flow. It runs four balanced Williams-order rounds over
two Edge variants/Nginx/OpenResty, plus one preceding direct-origin calibration
per matrix cell: 64 proxy cases and four direct cases per CPU budget. Input
`xdp_udp_comparison=wake` compares worker/readable with bridge statistics;
`rx-stats` compares full-stats/bridge-stats with both using worker wakeups.
`idle-yield` compares eager/cooperative with both using worker wakeups and
bridge statistics. The report must confirm all three selected controls from
each node's JSON; source and binary hashes must remain unchanged.
Every case has 1000 warmup round trips per flow. CI measures 40000 round trips
per flow at concurrency one and 10000 at concurrency sixteen, addressing the
short single-flow samples in the initial matrix. The client validates the
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
source and binary digests, commands and failed attempts are uploaded. Actual
configuration YAML and baseline server configurations are retained too. CPU
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

## Syscall diagnostics

Dispatch with `xdp_udp_profile=true` and `xdp_udp_comparison=rx-stats` for a
separate instrumented matrix. It retains four balanced rounds and 1/2 proxy CPU
budgets, with 2,000 timed requests for a single flow and 1,000 per flow for 16
flows; warmup stays at 1,000 per flow. These diagnostic iterations are distinct
from the uninstrumented performance matrix. All byte, sequence, reuse, redirect,
affinity, queue/error and cleanup checks remain required.

The counts-only `strace -f -c -w` tracer attaches to the observed proxy process
tree after warmup. `/proc` must identify it as tracer for every observed thread
before GO and after DONE; it detaches before idle sampling. Raw summary, exact
signal/exit status, attachment timestamps/thread IDs, syscall/error counts and
aborted traces are retained. The helper validates total counts and completed
coverage; an early exit fails even with status zero or SIGINT. A requested
SIGINT can yield zero or `-SIGINT` after normal detach; both require the retained
summary and proof that all observed threads detached. The real-process helper
tests cover counts, thread coverage, early death and cancellation.

The `afxdp-udp-syscalls-*` artifact is separate. Its timing and CPU measurements
are perturbed by ptrace and must not be quoted as native throughput, latency or
capacity. Syscall wall time includes waiting and is summed across threads;
it is not CPU consumption. Counts include snapshot boundary overhead, exclude
warmup and the 2.2-second idle observation, and are retained without filtering.
No application buffers or request contents are logged by the tracer.
