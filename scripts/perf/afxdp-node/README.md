# Full-node XSK wake integration gate

The CI `workflow_dispatch` input `xdp_node_smoke=true` builds one pair of
`zelvik-edge` / `h3-probe` binaries, then runs the existing full application
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
