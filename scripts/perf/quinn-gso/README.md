# Native Quinn GSO diagnostic

`ZELVIK_EDGE_QUINN_NATIVE_GSO=auto` is the default. An explicit `off` is accepted
only with `ZELVIK_EDGE_H3_BACKEND=quinn-native` and
`ZELVIK_EDGE_H3_TRANSPORT_PROFILE=matched`. Unknown values and combinations that
would ignore the option fail configuration validation. The normal shared Quinn
backend and production profile keep their current behavior.

The implementation uses `quinn::TransportConfig::enable_segmentation_offload`.
Pinned quinn-proto 0.11.17 limits `poll_transmit` to one datagram when disabled;
no custom socket wrapper, receive/GRO changes, poller changes, packet splitting
or fallback is introduced. Native listener startup reports the selected mode.
The diagnostic does not yet establish that GSO caused the two historical curl
56 errors, and disabling GSO is not proposed as a production performance win.

The workflow builds one actual production node after all library/binary tests
and all-target Clippy. Both modes use that same executable, kernel, CPU budget,
matched transport and explicit Retry-off configuration. The two conditions
match the historical failures: 32 KiB HIT and 128 KiB OFF. Four alternating
pairs each include 16,384 fresh connections and a five-second reuse control.
There are 32 complete cells, with 262,144 planned fresh requests, plus reuse
traffic. Each cell restarts the node and tmpfs; all targets execute alone after
compilation and image preparation finish.

The harness confirms the effective GSO mode from the native listener's startup
line. Only that module's lifecycle info is enabled; the rest of the node remains
at WARN. The verifier checks raw protocol/body/connection/error records, cache
and origin counts, CPU/P99, masks and host steal. Failed and incomplete cells
receive no throughput ranking. A zero-error short run does not establish that
a rare failure has been eliminated or replace a 24/72-hour soak.

Artifacts retain original traffic, startup mode observations, exact executable
hashes and failures. Public evidence excludes private TLS material, runtime
overrides, controller stores and whole node logs. The earlier failures remain
in `docs/performance-h3-native-2026-11-04.md`.

```sh
python3 scripts/perf/quinn-gso/verify.py /path/to/extracted-evidence \
  /path/to/extracted-binaries/.local/quinn-gso-binaries
```
