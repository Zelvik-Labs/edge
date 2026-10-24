# Issue 34 memory validation

This document describes the runnable harness in `scripts/memory`, the Rust
contract tests in `tests/issue34_memory.rs`, and the staged workflow
`.github/workflows/memory-soak.yml`. It is not a checklist that replaces those
tests.

## Coverage lanes (read this first)

| Lane | Target | Status |
| --- | --- | --- |
| Screen (short) | `target/debug/bench-defense` | Automatic 5 min (300 s) screen. Looser 64 MiB/h budget, report marked `screen_only`; can never close Issue 34. |
| Screen (full-Edge) | real `Zelvik-Edge` + fake control plane | Automatic 15 min (900 s) screen with real production startup, HTTP/1.1 + HTTP/2 TLS, node status and UNAVAILABLE recovery. 8 MiB/h budget, `screen_only`; not a leak closure. |
| Component long | `target/debug/bench-defense` | Manual `workflow_dispatch` (`run_long_component=true`) 4 h component soak with the strict 0.5 MiB/h RSS+Anon gate. Never scheduled. **Not full-Edge coverage.** |
| Full-Edge long | real `Zelvik-Edge` + fake control plane | Manual `>=48 h` dedicated self-hosted soak only; 0.5 MiB/h gate and split baseline. This is the closure lane. |

The stated 5 min / 15 min are soak durations. They exclude the first Rust
cold build and eBPF build, so job wall-clock is longer. `rust-cache` is reused
between jobs; the two jobs still build separately, and sharing a build-once
artifact is an out-of-scope optimisation for another round.

`bench-defense` constructs `EdgeProxy` + `HttpProxyManager` with real
HTTP/TLS/TCP/UDP listeners and local config injection. It exercises the same
allocator, handoff, and listener code, but it does **not** execute the
production startup path: `Zelvik-Edge` loads `$ZELVIK_EDGE_HOME`, opens the node
RPC client, receives server/listener configuration from the control plane,
writes its own pid file, and only then binds listeners. Every `bench-defense`
report records `scope.component_lane=true`, `scope.production_startup=false`,
and `scope.full_edge=false`, and the workflows assert those values. Do not cite
a green component or screen run as full-Edge evidence.

The full lane uses `examples/issue34-controller.rs`: a fake ControlChannel +
NodeControl backend that serves a real HMAC-signed snapshot and node status to
the real `Zelvik-Edge` binary started from a throwaway `ZELVIK_EDGE_HOME`. Both
the `short` job and the automatic 15 min `full-edge` job run a mandatory
`full_edge.py smoke` (real startup, HTTP/1.1 and HTTP/2 over TLS with CA/SNI
verification, NodeControl health, full `resourceGovernor`, telemetry
retry/recovery and terminal rejection). The 15 min full-Edge screen and the
manual `>=48 h` dedicated job run the same process, including the retryable
UNAVAILABLE -> durable-recover cycle. The fixture never falsifies
`workerReclaim.coversAllWorkers` (the real Edge honestly reports unhooked
Pingora workers). The full-Edge screen and long jobs **do** pass
`--require-node-status`: every
polled sample must contain the full `resourceGovernor` field set, missing
fields and read errors are hard violations, and generation/cycles must not
decrease. `coversAllWorkers=false` is not conflated with full worker coverage:
it is reported as an explicit `node_status.coverage_gap` limitation, and
`--require-complete-worker-coverage` is the separate opt-in that turns that
gap into a failure once the production Pingora hook exists. Node-health
samples (`>=10`) are required regardless.

## Telemetry closure (retryable mode wired)

The fixture now has the retryable entry parameter: `--telemetry-mode
unavailable` returns gRPC `UNAVAILABLE` (14) and `--recovery-seconds N`
switches to the durable success backend `N` seconds before the run ends, then
waits for the retained backlog to be acknowledged. The Edge must keep complete
batches in its bounded on-disk store while the backend is down; the store is a
single sparse file under `$RUNNER_TEMP` for the test run, never a production
path.

The mandatory `short` and `full-edge` startup smokes assert **both**:

* `telemetry-retryable-recovery`: real TLS requests produce batches, the Edge
  retries them against `UNAVAILABLE`, and after the backend returns it
  acknowledges at least the sequence observed before recovery with a real
  receipt (`pending_after=0`, `acked_sequence >= max_sequence`, bounded
  `max_batch_bytes <= 8 MiB`). Core and detail telemetry lanes have
  independent sequence spaces and both must drain: pending is
  `core_pending + detail_pending`, so a detail ack cannot mask a core
  backlog; and
* `telemetry-reject`: terminal `UNIMPLEMENTED` (12), `served=false`.

The automatic 15 min `full-edge` screen passes
`--telemetry-mode unavailable --recovery-seconds 60`, and the manual
`>=48 h` dedicated job passes its own recovery window. Both assert
`telemetry_recovery.status=pass`, mode `unavailable->success`,
`pending_before >= 1`, `pending_after=0`,
`acked_sequence >= max_sequence`, bounded batch bytes, bounded scalar status,
and `telemetry_store` under `$RUNNER_TEMP`. A synthetic/fake ack or an
unbounded backlog fails these assertions. The telemetry phase does not change
the memory gates: 64 MiB/h for the 5 min short screen, 8 MiB/h for the 15 min
full-Edge screen, and 0.5 MiB/h only for the manual long lanes.

## Allocator contract

`src/bin/bench-defense.rs` is a test-only binary. It now declares the same
`#[global_allocator] mimalloc::MiMalloc` as `src/main.rs` and calls
`zelvik_edge::memory_reclaim::configure_allocator()` before any runtime or
worker thread starts. The allocator owner's contract (`src/memory_reclaim.rs`)
is v3 slot 35 `mi_option_page_reclaim_on_free=1` and slot 26
`mi_option_disallow_arena_alloc=0`, with `mi_version` and readback assertions.
`tests/issue34_memory.rs` re-asserts that contract from the test process.

The optional `BENCH_ALLOC_STATS_PATH` (bench-defense only) atomically writes
`mi_process_info` plus the raw `mi_stats_get_json` document once per second. It
is a convenience for the soak; the RSS slope gate always reads kernel
`/proc/<pid>/statm/status/smaps_rollup`, never `mi_process_info` and never the
allocator's own RSS estimate.

## What the soak measures

`scripts/memory/soak.py`:

* starts a bounded origin (`ThreadingHTTPServer`) and the target;
* drives continuous TLS traffic with SNI and CA verification;
* samples `/proc/<pid>/stat`, `status`, `smaps_rollup`, `maps`, `fd`, cgroup v2
  (`memory.current/max/high/swap`, `memory.stat`, `memory.events`) every
  interval, and verifies the PID starttime (field 22) has not changed;
* periodically verifies real HTTP/2 + SNI + CA trust with `curl --http2
  --cacert --resolve` (must report `http_version=2`, `ssl_verify_result=0`,
  status 200, payload hash match) and `openssl s_client` (`-servername`,
  `-alpn h2`, `-verify_return_error`, `-verify_hostname`);
* gates **total RSS and anonymous RSS separately**: an OLS slope on the
  post-warm-up series with a one-sided 95% Student-t upper bound, plus a
  Theil-Sen robust slope, must each stay below the budget. The strict long-lane
  budget is **0.5 MiB/h** (24 MiB over 48 h). A 4.5 MiB/h tail leak is a
  unit-tested negative case for that gate. Fast screens use their own explicit
  budgets: **64 MiB/h** for the 5 min under-`bench-defense` screen and
  **8 MiB/h** for the 15 min full-Edge screen. Those values are screen
  thresholds only; they never inherit or claim the 0.5 MiB/h long-lane
  closure. The `max-rss-mib` ceiling is an additional guard, never the gate;
* for the `>=48 h` dedicated lane, also re-gates the **second half** on its own
  and checks the half-to-half median **step baseline** against
  `budget * half_hours`, so a slow leak cannot be diluted over tens of hours;
* runs `N` fork-based anonymous-memory pressure cycles inside the ambient
  cgroup, requiring a `high`/`max` event per cycle, swap `0`, no `oom_kill`,
  and the same PID/starttime after recovery. Fast screens use 1-2 early cycles
  with a 15 s hold and a 10 s probe; the manual long lanes require at least 10
  cycles, a 60 s hold, and a 30 s **sustained recovery probe** (a single dip
  below the ceiling does not count);
* reads the controller-reported `resourceGovernor` snapshot from the fake
  backend (`--status-source`, file/JSONL/URL) and strictly validates
  `workerReclaim`, `cacheJanitor`, `metricValuesCache`, and
  `metricsAggregatorBytes` on **every** polled sample. A missing field or a
  read error is a violation, never zero and never swallowed;
  generation/cycles must not decrease. `coversAllWorkers=false` is reported as
  `node_status.coverage_gap` (with `unhookedPingoraWorkers`) instead of being
  treated as full coverage or skipping the counter checks. Only that subset is
  copied into the report; no host identifiers or secrets are printed. The RSS
  gate never consumes this snapshot.

Any missing `/proc` or cgroup field is `StatError`/`PressureError`, never a
silent zero. Sample, verification-result, error-sample, and violation arrays are
bounded.

## Local commands (Linux development host)

```sh
# Python harness tests. On non-Linux, the two live /proc tests skip; this
# forces them to fail instead:
ISSUE34_REQUIRE_LINUX=1 python3 -I scripts/memory/tests/run_tests.py

# Rust allocator/window/handoff tests (Linux-only; the allocator observations
# are serialized by a process-wide mutex even with two test threads):
CARGO_BUILD_JOBS=2 cargo test --test issue34_memory -- --test-threads=2 --nocapture

cargo build --bin bench-defense

# Full-Edge production-startup smoke (real binary + fake control plane):
cargo build --bin Zelvik-Edge --example issue34-controller
python3 -I scripts/memory/full_edge.py preflight \
  --edge-binary target/debug/Zelvik-Edge \
  --controller-bin target/debug/examples/issue34-controller
python3 -I scripts/memory/full_edge.py smoke \
  --edge-binary target/debug/Zelvik-Edge \
  --controller-bin target/debug/examples/issue34-controller \
  --report /tmp/issue34/full-edge-smoke.json

# Short SCREEN soak (5 min), 2 GiB / no-swap ambient cgroup. NOT closure:
sudo systemd-run --scope \
  -p MemoryMax=2G -p MemoryHigh=1536M -p MemorySwapMax=0 \
  --working-directory="$PWD" \
  -- python3 -I scripts/memory/run_soak.py \
     --target-cmd "$PWD/target/debug/bench-defense" \
     --target-kind bench-defense \
     --target-env BENCH_TLS_CERT=/tmp/issue34/certs/server.pem \
     --target-env BENCH_TLS_KEY=/tmp/issue34/certs/server.key \
     --target-env BENCH_ALLOC_STATS_PATH=/tmp/issue34/alloc-stats.json \
     --tls-port 8443 --tls-host localhost \
     --tls-ca /tmp/issue34/certs/ca.pem \
     --duration 300 --warmup-seconds 90 --min-span-seconds 150 \
     --budget-mib-per-hour 64 --anon-budget-mib-per-hour 64 \
     --screen-only --max-rss-mib 1024 \
     --pressure-cycles 1 --pressure-start-fraction 0.3 --measurement-clean-seconds 10 \
     --measurement-stabilize-timeout-seconds 180 --status-ready-timeout-seconds 60 \
     --pressure-hold-seconds 15 --pressure-settle-seconds 30 \
     --pressure-recovery-probe-seconds 10 \
     --stats-source /tmp/issue34/alloc-stats.json \
     --require-allocator-stats --stats-ready-timeout-seconds 30 --require-stats-samples 200 \
     --out /tmp/issue34/report.json

# Long closure profile (manual only): 0.5 MiB/h RSS+Anon, 1800 s warm-up,
# >= 10 x 60 s pressure cycles, 30 s sustained recovery probe.
```

The package-qualified `python3 -m scripts.memory.soak` form fails under
`python3 -I` (isolated mode drops the repository root from `sys.path`); use
`scripts/memory/run_soak.py`, which bootstraps the path like
`tests/run_tests.py`.

## What the workflows run

Trigger semantics: `push` and `pull_request` run the two automatic fast
screens (`short` and `full-edge`). A `workflow_dispatch` with
`dedicated_hours=0` runs those same fast screens; `run_long_component=true`
additionally selects the manual 4 h component soak; `dedicated_hours>=48`
selects only the self-hosted long full-Edge run. There is no `schedule`
trigger. The PR path filter includes the allocator, memory governor/plan,
worker reclaim, cache janitor, metrics, RPC node/stats/cache files,
`src/main.rs`, `src/lib.rs`, `build.rs`, `tests/issue34_full_edge.rs`,
`examples/issue34-controller.rs`, the control plane fixtures, and this
workflow, so the Issue 34 core changes cannot land without the gate. Only the
Issue 34 memory tests plus the real full-Edge startup/smoke are retained in
these jobs; the eBPF build is required and is not bypassed by disabling
features.

Short automatic gate (`ubuntu-22.04`, 60 min job timeout): Python tests with
live `/proc` required run **first** (raw threshold/missing-stat negatives
before any Linux build), workflow YAML parse, `cargo test --test issue34_memory`
with `CARGO_BUILD_JOBS=2` and `--test-threads=2` (exact seven test names and
`7 passed; 0 failed` asserted), `cargo build --bin bench-defense`, a
**mandatory full-Edge production-startup smoke** (`cargo build --bin
Zelvik-Edge --example issue34-controller` then `full_edge.py preflight` +
`full_edge.py smoke`), and finally a 5 min (300 s) bench-defense **screen**
soak. The screen report must say `screen_only=true` and can never be reported
as closure.

Automatic full-Edge screen (`ubuntu-22.04`, 90 min job timeout): real
`Zelvik-Edge` + fake control plane. It runs the same mandatory startup smoke
(HTTP/1.1 and HTTP/2 over TLS, CA/SNI/ALPN, NodeControl health, telemetry
retry/recovery and terminal rejection), then a 15 min (900 s) soak with
`--screen-only`, `--warmup-seconds 120`, `--min-span-seconds 300`, 8 MiB/h
RSS+Anon, two early pressure cycles (15 s hold, 10 s probe, 30 s settle), one
fixed PID, OOM/swap checks, strict `resourceGovernor` fields, and
`--recovery-seconds 60`. Its report must say `screen_only=true`; it is a fast
screen, not the 0.5 MiB/h closure.

Manual component long tier (`workflow_dispatch`, `run_long_component=true`,
4 h hosted job): `bench-defense` with `--warmup-seconds 1800`,
`--min-span-seconds 10800`, 0.5 MiB/h RSS+Anon, 10 x 60 s pressure cycles and a
30 s sustained recovery probe. The analysis requires `slope.samples >= 5000`;
the reported `expected_samples` comes from the active sampling window. This is
never scheduled.

Optional dedicated (`workflow_dispatch`, `dedicated_hours >= 48`, label
`issue34-dedicated`): preflight requires `RUNNER_ENVIRONMENT=self-hosted`, the
`/etc/zelvik/issue34-dedicated-runner` marker, no running production
`Zelvik-Edge`, and systemd cgroup support. It runs the **full-Edge** lane with
1 h warm-up, the second-half platform/step baseline, and 10 x 60 s pressure
cycles. The report assertions require wall clock `>= 95%` of the requested
hours and at least 90% of the expected samples, so a short integration run
cannot be presented as a 48-hour result.

The 0.5 MiB/h budget is used only by the manual 4 h component and `>=48 h`
dedicated lanes. The 64 MiB/h and 8 MiB/h automatic screens cannot cite it, and
a budget above 0.5 MiB/h would let the known 4.5 MiB/h tail leak pass. CI noise
makes a long run inconclusive/fail; it is not a reason to relax the threshold.

No workflow uploads artifacts, writes to external services, or prints secrets.
Reports and logs stay under `$RUNNER_TEMP`.

## Local verification performed

On macOS (development host), with the released build slot:

* `python3 -I scripts/memory/tests/run_tests.py`: 123 tests, `OK` with two
  Linux-only skips.
* `ISSUE34_REQUIRE_LINUX=1 python3 -I scripts/memory/tests/run_tests.py`:
  exits 1 with two `AssertionError` failures, proving the required tests cannot
  turn into green skips.
* The strict negatives are unit-tested: a synthetic 4.5 MiB/h series fails a
  0.5 MiB/h budget, and a flat RSS with 4.5 MiB/h anonymous growth fails the
  dual RSS+Anon gate.
* `CARGO_BUILD_JOBS=2 cargo test --test issue34_memory --no-run`: compiles
  cleanly (no warnings). The tests themselves are `cfg(target_os = "linux")`,
  so executing them is a Linux/CI action.
* `CARGO_BUILD_JOBS=2 cargo build --bin bench-defense`: compiles cleanly.
* `full_edge.py preflight`: `pass`, no missing prerequisites.
* `full_edge.py smoke` with the real `Zelvik-Edge` binary: `status=pass`,
  `scope.full_edge=true`, `scope.production_startup=true`,
  `edge_pid == pid_at_exit` (57229), `node_health_observed=true`, HTTP/1.1 and
  HTTP/2 checks ok with identical payload hashes, ALPN `h2` peer verified,
  NodeControl status ok, `telemetry-retryable-recovery` ok with independent
  core/detail lanes (`pending_before=3` = core 1 + detail 2, both
  `pending_after=0`, `acked_sequence=6`, `max_batch_bytes=1621`,
  `bounded_status=true`, 9.73 s), and terminal telemetry rejected with
  `UNIMPLEMENTED` (12), final mode `unimplemented`, `served=false`. Actual
  report: `/tmp/issue34-full-edge-retry-report.json`, summary
  `/tmp/issue34-full-edge-retry-summary.json`; the short-lane assertion block
  passes against that real summary.
* `.github/workflows/memory-soak.yml` parses as YAML; job/step structure was
  inspected for duplicate or malformed steps.
* The actual `Validate workflow YAML and lane contract` shell step was extracted
  from the YAML and executed with `bash -euo pipefail`: `workflow lane contract
  ok`. A mutated long lane with a 64 MiB/h budget fails with
  `does not use the strict RSS budget`, and removing
  `--telemetry-mode unavailable` from full-edge fails with
  `full-edge retryable telemetry is missing ...`. The old self-matching
  `grep` guards are gone; the check inspects the parsed job `run` strings and
  skips its own step.
* The **actual** 300 s short and 900 s full-edge assertion blocks were
  extracted from the workflow and run against reports whose slopes came from
  the real `scripts.memory.soak.analyze_pair`:
  - 900 s / 2 s, 120 s warm-up, 300 s minimum span, 8 MiB/h, two early pressure
    cycles: exit 0 (`expected_samples=421`, `samples=451`, slope `samples=391`,
    slope `span_seconds=780`).
  - 300 s / 1 s, 90 s warm-up, 150 s minimum span, 64 MiB/h, one early cycle:
    exit 0 (`expected_samples=276`, `samples=301`, slope `samples=191`, slope
    `span_seconds=190`).
  - The full-edge block exits 1 for `pending_before=0`, a short ack, an 8 MiB+
    batch, or a telemetry store outside `$RUNNER_TEMP`. The short block exits 1
    when the `telemetry-retryable-recovery` check is absent.
* The manual long-lane blocks keep the 4 h synthetic window tests and the
  4.5 MiB/h RSS/Anon negative case; those lanes remain the only ones that
  claim the 0.5 MiB/h gate.
* `expected_samples` is derived from the tracked active sampling window
  (`sampling_span_seconds - pressure_blocked_seconds`), not requested uptime.
  Pressure cycles stay synchronous so the fork-based allocator never forks a
  multi-threaded process; the blocked window is reported instead of pretending
  the loop kept sampling. All lane assertions require
  `samples >= expected_samples * 0.9`.

Not verified locally: actual Linux execution of the Rust tests, the HTTP/2
soak, the cgroup pressure cycle, or any GitHub Actions run. Those require the
Linux runner started by main after push.

## What main must do

1. Merge the allocator changes (`src/memory_reclaim.rs` and related) and this
   branch's harness, Rust tests, test-only `bench-defense` edit, full-Edge
   fixture (`examples/issue34-controller.rs`, `tests/issue34_full_edge.rs`,
   `scripts/memory/full_edge.py`), workflow, and doc. `src/lib.rs`,
   `src/metrics.rs`, and `src/rpc/node.rs` remain main-owned.
2. Commit and push, then open the PR. No commit, push, or dispatch was done
   from this branch. The short workflow cannot run until the branch is on
   GitHub and the `ZELVIK_DNS_DEPLOY_KEY` / `PROTOCOL_DEPLOY_KEY` secrets are
   available.
3. The full-Edge smoke and 15 min full-Edge screen are mandatory automatic
   jobs. The 4 h component soak (`run_long_component=true`) and the `>=48 h`
   dedicated soak are manual `workflow_dispatch` tiers only; there is no
   schedule. The optional `node_status_source` dispatch input
   is only for the component lane; the full-Edge fixture supplies its own
   status endpoint. Never point either at production.
4. Do not present the short screen or the component lane as full-Edge
   coverage. Only `full-edge` (startup smoke or 15 min screen) and the
   dedicated long lane report `scope.full_edge=true`; the workflows assert
   that distinction, and the automatic full-edge report is still
   `screen_only=true`.
5. **Dependency-lock boundary (Geo WIP).** The pinned DNS runtime
   `e357bf64bad28b3dc519d317e396af71fe2a884e` predates
   `GeoRecord.subdivision_ids` / `has_city` / `has_country`. The memory patch
   must be staged selectively and must **not** include the Geo work in
   progress: `src/metrics/analyzer.rs`, `tests/geo_agent.rs`, and
   `tests/testdata/**` (the three `*.mmdb` fixtures). Those need a newer
   runtime release and land separately against their own released pin. Do
   **not** move or change `DNS_REF` to make the Geo tests compile — that
   would mask the dependency instead of recording it. `src/metrics.rs`
   (the one-line aggregator-stat addition) and `src/rpc/mod.rs` (the
   `metric_stat_cache` module declaration) are memory-lane changes, not Geo,
   so they are outside this exclusion set; per item 1, main still owns merging
   them. A local snapshot of the
   Geo WIP is at `/tmp/issue34-geo-wip-backup-20261024-182548/`
   (`MANIFEST.txt` records worktree, HEAD, time and the analyzer diff); the
   originals were left untouched.
