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
     --pressure-cycles 1 --pressure-start-fraction 0.1 --measurement-clean-seconds 10 \
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
5. The integrated 26.10.5 branch uses the same DNS revision as the
   26.10.4.9 CI and release workflows:
   `aca0f3bb61bc498682b10900b4886709a2c248b8`. The older issue34 branch's
   Geo dependency exclusion no longer describes this integration. Access-log
   analyzer changes are part of this release; unrelated changes in other
   worktrees remain outside it.

## Integrated 26.10.5 verification

The local verification section above records the original issue34 branch,
not the final release candidate. The integrated branch has 160 Python harness
tests: 158 pass with two Linux-only skips on macOS, and all 160 pass on the
Linux test server with `ISSUE34_REQUIRE_LINUX=1`. Rust fixes also cover metric
plan lifetimes, charge transfer, durable cumulative snapshots and late events,
telemetry quarantine, and access-log enrichment. Final Linux acceptance and
release provenance are recorded separately; no 4 h or 48 h soak is run for
this release. Short observations cannot close the long-term memory claim.

### 2026-10-31 request ownership diagnostic

The candidate version was corrected from `26.10.4.10` to `26.10.5` using
`license/shared/calver.ts`: October 31 is week 5, and the Edge release bucket
has no release in that week yet. This version correction does not change the
memory verdict. The automatic component and full-Edge screens at `ad4026e`
still fail their original RSS/Anon thresholds; no threshold was relaxed.

A separate diagnostic on the 43 test host used the existing `f9f4598` release
binary, a 2 GiB cgroup, and process-only `MIMALLOC_ALLOW_THP=0`. It does not
validate a newly compiled `26.10.5` binary. It traced all 8–100 KiB allocation
requests, frees, and reallocations for 165 seconds, with 60 seconds of TLS
POST traffic followed by idle observation. There were 11,311 successful
requests and no load or profiler errors. The tracked `BodyReader` and
`process_http1_stream` net allocations returned to zero after traffic stopped.
RSS was 135.629 MiB when load stopped and 134.125 MiB at 150 seconds, with
intermediate fluctuations. The main remaining traced allocation was a
1,672,704-byte Mace page-map allocation; its recycling pool is capped in the
pinned Mace source. This diagnostic does not cover every allocation size or
prove that the full process has no leak.

Raw evidence: `/root/issue34/diag-request-idle-xBNMQZZl` on the test host and
`/tmp/edge-cycle-request-idle-data.json` locally. The diagnostic processes and
their temporary home were removed; evidence and build caches were retained.

### Large-allocation follow-up

A 195-second diagnostic on the same binary and test host traced allocation
requests above 100 KiB across 60 seconds load, 60 seconds idle, and 60 seconds
load again. Both load phases completed without errors (11,329 and 11,326
requests). The large transient allocation net returned to zero between load
phases. A retained 418,848-byte table resolved to sysinfo's process HashMap.
The second load ended at 139.891 MiB RSS versus 135.590 MiB for the first;
this remaining increase is unexplained by the traced large live allocations.
No leak-closure or slope-gate pass is claimed. Raw evidence is
`/root/issue34/diag-large-cycle-VqZLR4nT` on the test host and
`/tmp/edge-large-cycle-data.json` locally. Processes and the temporary home
were cleaned up.

A separate 135-second trace covered allocation requests below 8 KiB, with
60 seconds of load (11,316 requests, zero errors) followed by idle. It reported
no profiler errors. RSS fell from 139.672 MiB at load end to 129.898 MiB at
120 seconds. Remaining traced small allocations included roughly 402 KiB of
sysinfo process strings in addition to the process table above, plus Mace and
local tree cache objects. This supports removing unused process enumeration
from CPU status reporting, not a conclusion that the full memory gate passes.
Evidence: `/root/issue34/diag-small-idle-vu1sKCKP` and
`/tmp/edge-small-idle-data.json`. Processes and the temporary home were cleaned.

### CPU-only repair acceptance and wrapper output

At `ef80a63`, the normal Linux CI passed. The component screen still failed:
RSS/Anon upper bounds were 233.292/232.706 MiB/h against 64, over 197.784
seconds. The 15-minute full-Edge screen had a different result: its actual
memory report passed, with RSS/Anon upper bounds -8.269 MiB/h against 8 over
682.305 seconds, 131,703 measured requests with zero errors, and peak RSS
374.008 MiB. Telemetry recovery passed (150 pending batches to zero in
19.182 seconds); the controller and soak exit status passed too. The job
then failed parsing the wrapper stdout: the child had printed its report
before the wrapper's JSON summary. Child stdout now goes to stderr, retaining
its logs and exit status while reserving stdout for one wrapper summary.
The regression covers both child success and failure; restoring the old
output routing reproduces JSONDecodeError. This does not relax a gate.

The repaired release binary on the 43 host still failed the unchanged
300-second, 2 GiB, process-THP-off comparison: RSS/Anon upper bounds
74.333/73.364 MiB/h against 64, peak RSS 144.738 MiB, 58,431 requests without
errors. Keeping telemetry healthy throughout another diagnostic also failed
(84.909 MiB/h, 58,431 requests without errors). Thus an unavailable telemetry
backlog is not necessary for the remaining short-window growth. These
observations do not establish long-term closure or justify merging with the
component gate failing. Raw evidence is retained in
`/tmp/edge-ef80a63-full-report.json`, `/tmp/edge-ef80a63-full-summary.json`,
`/tmp/edge-cpu-monitor-screen-result.json`, and the test host's
`/root/issue34/healthy-telemetry-koi2pgYr`. Test processes were cleaned up.

### Native allocator counter ingestion

The component writes `mi_stats_get_json` under `mimalloc`, while the sampler
previously searched only root-level optional counters. The pinned v3 JSON uses
`current`/`peak`/`total` count objects and scalar purge/reclaim counters. As a
result, the previous component reports preserved process-info counters but
omitted this native breakdown. The reader now retains a bounded set of native
counters and current/total bin counts under `mimalloc.*`; bin counts are not
labelled as bytes and per-bin peaks are not added. Required process counters
and kernel RSS/Anon gates are unchanged. The new nested-document regression
fails with the old reader (missing native counter) and passes with the fix;
164 harness tests ran on macOS, 162 passed and two Linux-only tests skipped.
Independent review found no blocker. Availability and precision of native
counters still depend on the allocator's compiled statistics level.

A separate source review found that `bench-defense` builds its own Tokio
runtime without the worker collect hooks installed by production startup.
It also omits the production reclaim monitor and cache janitor. This is a
component lifecycle difference, not evidence that it explains the measured
RSS growth. A subsequent component-only change installs the existing worker
hooks and reports start/park/stop progress; it still does not claim full-Edge
startup coverage or add the production janitor/reporters. See the paired
THP-on results below.

### Same-process allocation census

The CPU-only release binary was traced across all Rust mimalloc request sizes
in one 255-second diagnostic: 180 seconds load (34,007 requests, zero errors)
then 60 seconds idle, in the same 2 GiB scope with process-only THP disabled.
At the 60-second trace sample, `TinyUfoL1::build_cache` had retained
11,989,350 + 4,194,304 bytes (15.434 MiB), unchanged through the final sample.
This identifies a concrete cache-metadata allocation step; it does not prove
that the step explains the failed slopes. Source constructs TinyUFO capacity
from the byte budget and replaces it during budget rebuilds. Mace map creation
again retained 1,672,704 bytes. RSS at load stop was 133.441 MiB and fell to
126.938 MiB by 240 seconds.

The profiler also reported pointer-overlap detections (13 in the periodic
samples, 406 in the final dump). Its aggregate outstanding-byte curve is
therefore not accepted as exact live-heap accounting or evidence excluding a
leak. Pre-attach objects and native library allocation paths are also outside
its coverage. Raw evidence is `/root/issue34/diag-allocation-census-3TYYfyK1`
on the test host and `/tmp/edge-allocation-census-evidence` locally. The
processes and temporary home were cleaned up; no acceptance gate is changed.

A 195-second follow-up moved realloc's old-pointer tracking removal to entry
and restored it on allocation failure, avoiding a known window where another
thread could reuse an already freed address before return-time bookkeeping.
It completed 120 seconds load (22,665 requests, zero errors) and 60 seconds
idle. The cache-construction step again appeared near 60 seconds and stayed
constant: 10,997,838 + 4,194,304 bytes in this run. RSS fell from 137.660 MiB
at load stop to 127.273 MiB at 180 seconds. Pointer overlaps remained (40 in
periodic samples, 680 in the final dump), so this follow-up still does not
validate aggregate live-byte accounting. No further repeat is counted as
acceptance. Evidence: `/root/issue34/diag-allocation-census-v2-QhvkZcet` and
`/tmp/edge-allocation-census-v2-evidence`; processes and temporary home removed.

The source chain is the global `LazyLock` in `src/cache_manager.rs`, accessed
by janitor maintenance, then `HybridStorage::new` / `TinyUfoL1::build_cache`.
TinyUFO preallocates estimator slots and its bucket table from capacity.
Previously recorded CPU-only and healthy-telemetry 300-second samples both
show `cacheJanitor.rebuildsApplied = 0` throughout. The observed construction
step is therefore consistent with delayed first initialization; those samples
do not support repeated cache rebuilds as an explanation of their failed gates.

### THP-on component lifecycle and calibrated page accounting

THP remains enabled. The test host uses `madvise`; no global setting was
changed and the following runs do not set `MIMALLOC_ALLOW_THP=0`.
The component runtime now installs the production worker hooks. Its focused
runtime regression passed, failed when hook installation was removed, then
passed after restoration (one test, no skip). Native v3 count objects are
signed `int64_t`: a real `pages_abandoned.current=-1` initially stopped stats
readiness. The reader now preserves such native values without clamping;
process fields and kernel RSS/Anon remain nonnegative and strict. The
old-reader negative control fails on that captured shape. Python harness
verification ran 166 tests (164 pass, two Linux-only skips on macOS), and the
affected Linux release Clippy check passed. Independent review found no blocker.

The matched 300-second component runs used 2 GiB/no-swap, the same one pressure
cycle, eight load workers, 90-second warmup, at least 150 measured seconds,
and unchanged 64 MiB/h gates. Both failed: baseline RSS/Anon upper bounds
414.222/413.628 MiB/h, peak 201.891 MiB; hooked runtime 237.983/237.400 MiB/h,
peak 192.109 MiB. Measured requests were 26,641 and 26,511 respectively with
zero measured errors. The hooked process reported 75 park collections and
five stop collections. One pair is not a stable causal performance percentage
and the component gate remains failed. Evidence:
`/root/issue34/component-baseline-thp-3oF1LZ2J`,
`/root/issue34/component-hooks-thp-38LmFMG6`, and the corresponding local
`/tmp/edge-component-{baseline,hooks}-thp-result.json` files.

Before further live-object diagnosis, known-allocation fixtures calibrated
the probes: four retained 1 MiB requests totalled exactly 4 MiB in allocation
tracking, then zero after release. The independent page metadata reader
reported 4.25 MiB (allocator size-class rounding) then zero pages, even while
16 MiB THP remained resident. A second fixture held 100 64-byte blocks on one
thread and freed them from another: live bytes were exactly 6,400, 3,200, and
zero while page `used` stayed at 6,400; subtracting the deferred free list was
therefore necessary and verified. Calibration evidence is retained locally
in `/tmp/edge-page-live-calibration.log` and `/tmp/edge-deferred-calibration.log`.

A real Edge run then used the same THP-on 2 GiB scope for 240 seconds load
(46,395 requests, zero errors) and 60 seconds idle. Rounded arena live bytes
at 30/60/120/180/240/300 seconds were
162.962/164.749/158.976/160.010/159.033/158.677 MiB. RSS was
266.863/247.090/243.719/271.074/274.438/262.012 MiB, with THP
194/174/158/196/190/174 MiB. Empty pages rose from 182 to 280 at load stop;
retired pages from 122 to 174 by idle end. No deferred-list consistency
violations occurred. Each metadata snapshot paused the fixture for 25–45 ms;
this is diagnostic evidence, not a latency benchmark. It covers arena pages
and size-class rounding, not native allocations or a long-term leak guarantee.
Evidence: `/root/issue34/diag-thp-pages-FCsF1ay6` and
`/tmp/edge-thp-pages-evidence`. Its processes and temporary home were removed.

Mimalloc keeps some retired pages for 16 non-forced collection cycles. A
30-to-five-second worker cadence trial completed another 240-second load
(46,552 requests, zero errors) plus 60 seconds idle. Final retired pages were
175 versus 174 in the original diagnostic; the trial did not establish a
retirement benefit. Its h2load performance run reported zero measured requests
in every round, so those results are invalid performance evidence despite
exit code zero. The cadence change was withdrawn; production retains 30 seconds.
Raw trial evidence is `/root/issue34/diag-thp-pages-1hnR698P` and
`/root/issue34/perf-{baseline-30s-8j0zubc3,candidate-5s-vzh7z1jx}`. Processes and
temporary homes were removed. An additional process-only `allow_thp=2`
purge-alignment experiment failed initial HTTP2 admission before measurement;
no valid memory result or product default was taken from it.


### Short-window metric ownership repair (2026-10-31)

An hourly RSS slope is an observation, not an expected growth rate. With fixed
sites/dimensions and healthy delivery, retained metric history must not grow
with uptime. The source audit identified actual long-lived owners: clean
custom snapshots retained for at least an hour/twice the configured period,
a day/IP set loaded from disk and retained for multiple days, and a second
per-server IP set retained for the metric-instance lifetime. Successful upload
released none of the latter two sets.

The candidate now collects custom metrics only by event minute. Day/week/month
are backend ClickHouse output intervals. A successful durable spool/quarantine
handoff releases the snapshot row and charge in the same reporter tick; the
post-handoff checkpoint persists deletion with the epoch/revision watermark.
Further events in that minute receive a new incarnation/hash, while retrying
an already checkpointed row preserves its old hash/revision.

Exact visitor identities use bounded `(site, event minute, IpAddr)` batches,
independent of optional/sampled access logs. Both request-start hooks record
each request, including reused connections. Accepted identities release RAM
on durable enqueue; refused chunks retain their charge and restore unsent
identities. Edge no longer writes/reloads daily UIP keys or keeps a second
lifetime IP set. Fixed map capacity may be reused; no periodic shrink or
allocator-cadence change is used to hide growth. Existing historical local
keys/packed slots remain untouched.

Built-in request statistics also carry event minutes. The backend's new
`server_request_stat_delta` preserves distinct batch/node/minute/dimension
contributions and applies FINAL before sums. A one-time seed preserves
surviving old rows with their original coarse timestamp. Historical values
already overwritten by the old replacing key cannot be reconstructed.

Validation so far: 71 affected RPC/snapshot tests passed before the built-in
changes, including 240 durable handoff/restart cycles with zero retained rows
and unique hashes. The Python harness ran 168 tests: 166 passed, two Linux-only
checks skipped on macOS. Real RKE-dev ClickHouse tests passed calendar
minute/day/week/month rollup, UTC+8 month boundaries, revision/incarnation
replay, exact IP union across nodes/minutes/months, distinct same-minute
request deltas and idempotent historical seed. This is ownership/storage
correctness evidence, not a long-term full-heap leak-closure claim. Final
Linux tests, benchmark and CI results must be recorded separately.

### Short-window acceptance and receipt-fixture correction

Linux affected suites completed 76 metric/storage tests and 72 RPC tests, with
no failure or skip. Three ownership negative controls failed after removing
snapshot release, IP charge release, or event-minute identity respectively;
restoring the source passed both suites. The storage integration completed
eight tests. All-target Clippy passed after updating removed APIs in the memory
comparison/storage benchmark and fixing the new main initialization warning.
The Python harness completed 169 tests (167 pass, two Linux-only skips on macOS).

The first short component CI screen at `33c0c4b` passed its RSS/Anon bounds
(upper95 6.919 MiB/h against 64), with 165 measurement samples over 164.659
seconds and zero measurement errors. Its overall verdict remained failed:
only 196 allocator samples were read against the required 200. Stabilization
recorded RSS through its callback but omitted the allocator read. Moving that
read into the common sample callback keeps the duration, warmup, minimum span,
200-sample requirement and all budgets unchanged. The new clock-driven regression
passes with 100 reads plus initial readiness; the old sampler fails with 41.

The same source's full-process CI screen failed: no stable plateau within
180 seconds, final trailing rate 21.729 MiB/min, and RSS/Anon upper95
79.378 MiB/h against 8. It measured 534.138 seconds/267 points, 101,339
successful requests and zero measurement errors. Snapshot entries and estimated
bytes were zero; that does not close the independent process-residency failure.

A subsequent real-process check exposed an acceptance-fixture bug: its durable
store returned `backend=issue34-fixture-durable`, while both the real API and
`Spool::acknowledge` require `direct`. Backend receipt counters reaching zero
therefore did not prove that Edge had retired its local queue. `1abf37f` corrects
the fixture and adds real Spool -> controller fsync -> Spool ACK coverage. The
regression passed, failed with the old backend label, and passed after restoring
the correction. Production receipt validation was not weakened.

With the corrected fixture a real Edge completed 5,000 requests without errors,
kept one PID, and released day/month-configured metric projections to zero
checkpoint rows and zero estimated/pending bytes. Exact IP observations also
reached zero without admission refusals or enqueue failures. Revision watermark
remained 2. The workload used 40 HTTP/2 connections, 125 requests each, within
the existing 200 streams/30 seconds churn limit. An earlier eight-connection
attempt hit that limit and is a failed load check, not performance evidence.
Evidence: `/root/issue34/short-state-process-l7e115cd`.

A separate calibrated THP-on observation used 2 GiB, memory.high=1.5 GiB,
no swap, 180 seconds load and 60 seconds idle. It completed 34,917 requests
without errors. At 0/30/60/120/180/240 seconds rounded arena live bytes were
162.235/162.701/158.679/158.871/158.019/157.832 MiB, while RSS was
244.098/274.352/278.840/279.996/292.387/287.477 MiB and THP was
196/198/184/186/224/214 MiB. Final metric rows/bytes and pending IPs were zero;
combined metric aggregation was 304 bytes. Metadata snapshots paused this
fixture for 21-50 ms and had no deferred-list inconsistencies. This distinguishes
live arena state from residency in this window; it is not a latency benchmark
or whole-heap leak guarantee. Evidence:
`/root/issue34/short-state-live-pages-be479nvi` and
`/tmp/edge-short-state-live-pages-evidence`. Processes and temporary homes were
removed. Formal Edge release still requires the remaining process and CI gates.

The corrected receipt source `1abf37f` passed the normal PR Rust test/Clippy
workflow (`41186523783`). Its memory workflow (`41186523841`) still failed:
component allocator samples reached 261, but effective measurement span was
119.379 seconds against 150. Full Edge RSS/Anon upper95 was 234.205 MiB/h
against 8, over 535.897 seconds/268 points with 103,099 successful measurement
requests and zero measurement errors. The 180-second stabilization timed out;
its final trailing rate was -12.978 MiB/min, whose absolute value exceeds 8.
Metric snapshots remained empty. A negative slope during recovery is not a
stable plateau and does not waive this failure.

A matched three-round, alternating performance screen compared `34c29d1` to
`1abf37f`, using the same host/toolchain/dependencies and release profile
(LTO off, 16 codegen units, jobs=2). Both arms completed 90,000 requests without
errors. Median baseline/candidate throughput was 6374.685/6389.880 requests/s,
CPU 705.333/707.000 microseconds/request, and P99 14.002/13.795 ms. Each arm
used 30 fresh h2load batches of 1,000 requests/eight connections, within the
existing HTTP/2 churn policy. This short localhost screen found no material
regression; the sub-percent throughput/CPU differences are not evidence of
statistically significant improvement. Evidence:
`/root/issue34/short-state-paired-1tgeyshg/report.json` and
`/tmp/edge-short-state-paired-performance.json`.

A completed process-only `MIMALLOC_ALLOW_THP=2` experiment retained THP and
used the calibrated 180-second load/60-second idle workload. It completed
34,908 requests without errors. Final live bytes were 157.834 MiB, RSS
295.184 MiB and THP 254 MiB, compared with default 157.832/287.477/214 MiB.
It did not establish a residency benefit and was not adopted. Evidence:
`/root/issue34/short-state-live-pages-um2p5uc8`.

The next bounded diagnostic used two pressure cycles and a real unavailable
telemetry period followed by recovery. At 12/62/117/182/242/302/352 seconds,
rounded arena live bytes were 162.527/164.602/165.585/161.246/160.396/161.839/
163.361 MiB; RSS was 254.219/246.332/246.809/271.262/310.047/299.902/315.852
MiB. Capacity rose from 170.744 to 188.949 MiB, deferred-free bytes from 2.610
to 9.563 MiB, empty pages from 164 to 257 and retired pages from 125 to 163.
There were no deferred-list inconsistencies. The same PID survived, with no
cgroup max/oom/oom_kill events and no swap. The controller's pending telemetry
count recovered from 71 to zero in 19.608 seconds. Metric snapshots stayed at
zero; the final one pending IP is an active minute's observation, not a
retained historical set. This report still failed stabilization and had no
usable post-stabilization measurement span; 21,872 errors during deliberate
pressure are recorded, with zero measurement errors. Neither the script's
successful evidence capture nor its six-minute duration constitutes a passed
memory gate. Evidence:
`/root/issue34/short-state-pressure-pages-3pznr76x` and
`/tmp/edge-short-state-pressure-evidence-20261031`. Its Edge process and
temporary home were removed.

A calibrated page-presence/owner diagnostic then attributed RSS using arena
bitmaps, `/proc/pagemap` and each thread's FS-base identity. Its known-allocation
fixture reported exactly 6,400/3,200/0 live bytes across cross-thread frees and
matched all owned pages to the allocating thread. In the real two-cycle run,
RSS rose 250.891 -> 296.758 MiB, rounded live bytes stayed 162.525 -> 162.242
MiB, and resident completely empty pages rose 39.215 -> 69.930 MiB. The four
largest final empty-page owners were Tokio threads created after startup,
retaining 14.477/10.781/9.699/7.629 MiB of empty resident pages. The separate
Pingora Server/background threads had negligible page ownership. HTTP/TCP/UDP
accept loops actually execute on the application's hooked Tokio runtime;
configured Pingora thread counts cannot identify the source of this RSS.
Snapshots paused this diagnostic for 115-176 ms and are not latency evidence.
Its workload still failed stabilization and had no usable measurement span;
74,785 requests succeeded, with 15,557 deliberate pressure errors and zero
measurement errors. Evidence:
`/root/issue34/short-state-owner-pressure-uusru5gi`. Process and temporary home
were removed.

The blocking pool has start/stop hooks but no park hook. Telemetry replay,
append, receipt, quarantine and snapshot persistence use this pool, keeping
allocating threads alive after other threads have freed their objects. A
separate known-allocation reproduction retained 13.875 MiB of completely
empty resident pages with 1.922 MiB deferred frees. A non-forced collect on
the allocating thread moved these pages to pending purge; a forced collect
then returned their residency to zero. This supplies a concrete lifecycle
failure rather than a reason to change global purge frequency or disable THP.
Evidence: `/root/issue34/blocking-calibration-{DEFERRED,NORMAL_COLLECT,FORCED_COLLECT}.json`.

The follow-up source routes durable storage tasks through one blocking-work
helper that non-forcibly collects after the closure drops its local state,
including error and unwind exits. It preserves results and Tokio JoinError
semantics and leaves the request path unchanged. The real allocator regression
uses a one-thread blocking pool, cross-thread frees and an uncollected probe on
the same live thread. All five worker tests passed, including success/error/
panic cleanup. An isolated harness compiled the actual worker module against
the same linked dependencies: positive/restored passed, removing completion
collection failed with 2,015,232 retained bytes versus zero. Affected telemetry
38 tests and snapshot 11 tests passed without skips; affected Clippy passed.
The independent review of this small follow-up found no concrete defect in
result/error propagation or drop order.

A second matched performance screen compared the preserved `1abf37f` binary
with this completion helper (candidate binary SHA-256
`a32478ef716d69e3306c864f820115b18142d513dc02ed1b144395211b0dded0`).
Both arms completed 90,000 requests without errors. Median baseline/candidate
throughput was 6381.428/6424.012 requests/s, CPU 708.000/697.000 microseconds/
request, and P99 13.887/13.826 ms. This bounded localhost screen found no
material regression. Evidence: `/root/issue34/short-state-paired-m2mxjqgx` and
`/tmp/edge-storage-completion-paired-performance.json`.

With the same two-cycle six-minute diagnostic, candidate RSS at 12/117/182/
242/352 seconds was 228.484/224.277/231.402/241.129/270.430 MiB; rounded live
bytes were 150.775/153.917/155.413/154.548/157.492 MiB and empty-page residency
28.965/19.699/21.172/25.969/52.266 MiB. This is lower residency than the prior
diagnostic but not a passed memory gate: stabilization still timed out, final
trailing rate -44.72 MiB/min, with no usable post-stabilization span. Most
remaining empty pages were now owned by the original async worker threads;
blocking-thread completion does not resolve every worker lifecycle. Evidence:
`/root/issue34/short-state-owner-pressure-mf4g39vy`. The unchanged 900-second
full-process screen is required before this follow-up can replace failed gate
evidence; no threshold, stabilization requirement or warmup was loosened.
