# Unified Memory Governance

This node uses one runtime memory governor for connection admission, origin establishment, Pingora keepalive sizing, cache memory, Bloom filters, negative cache, QUIC staging queues, and background work.

## Priorities

1. Downstream connection establishment: HTTP/HTTPS, TCP, HTTP/3.
2. Origin establishment: HTTP cache misses, passthrough, TCP forwarding, TOA, PROXY protocol origins, and HTTP/3 origins.
3. Reused upstream keepalive pools.
4. Cache memory and admission filters.
5. Background cache work.

Under memory pressure, cache promotion and background jobs yield before request establishment and origin connection.

## Budgets

`src/memory_governor.rs` reads host or cgroup memory and derives:

- connection budget: downstream connections, H2 streams, H3 requests, origin connect permits.
- FD-equivalent pressure: process FD count plus admission pressure, used before memory usage becomes high.
- zero-copy relay budget: extra duplicated FDs, pipe FDs, pipe buffers, and blocking tasks for Linux splice relays.
- keepalive budget: Pingora upstream keepalive pool size per worker.
- cache budget: TinyUFO L1 memory cache.
- Bloom budget: adaptive Bloom filter growth limit.
- negative cache limit: bounded by Bloom scale and governor memory budget.
- cache read budget: bounded admission for small disk HITs read into memory.
- UDP queued byte budget: bytes waiting in passthrough session queues.
- QUIC/H3 budget: H3 datagram queued bytes, H3 per-connection streams, and pending QUIC route/reassembly bytes.
- background work limit: cache warmup, purge, janitor, and L2-to-L1 promotion.

On Linux, cgroup v1/v2 limits take precedence over host memory. `inactive_file` is subtracted from cgroup usage so budgets track the working set instead of charging reclaimable page cache as active heap. If no cgroup limit is present, the governor falls back to `/proc/meminfo` `MemAvailable`. The governor keeps a hard reserve headroom and refreshes memory snapshots with a short TTL to avoid hot-path sysinfo calls.

## Covered Paths

- HTTP/HTTPS listener admission: `AdmissionClass::HttpConnection`.
- TCP listener admission: `AdmissionClass::TcpConnection`.
- HTTP/3 listener admission: `AdmissionClass::Http3Connection`.
- H2 per-connection stream limit: `AdmissionClass::Http2Stream`.
- H3 per-connection request stream limit: `AdmissionClass::Http3Request`.
- L7 origin path: `proxy_upstream_filter` holds `OriginConnect` until Pingora reports the upstream connection is established or reused; request logging is only a fallback release point.
- SNI passthrough and TCP forwarding: hold `OriginConnect` only until the upstream socket, optional PROXY protocol header, and optional TLS handshake are established. Long-lived relay time is accounted by connection/session admission, not origin-establishment admission.
- TCP/SNI relay: read loops periodically re-evaluate pressure and close idle slow connections under `Elevated`/`High`/`Critical` pressure even if the connection entered `read` while pressure was normal.
- Linux zero-copy relay: requires a separate zero-copy permit and is automatically disabled under High/Critical memory or FD-equivalent pressure. The async copy path remains the fallback.
- TOA connect helper: guards connect/resolve/setup with `OriginConnect`.
- PROXY protocol custom L4 connector: guards custom upstream connect with `OriginConnect`.
- HTTP/3 origin connector: guards H3 origin establishment with `OriginConnect`.
- UDP passthrough: session queues are capped by item count and global queued bytes.
- QUIC UDP demux: pending route/reassembly buffers, adaptive route idle, and H3 shared UDP queue bytes are capped by the governor.
- HTTP/3 server: request permits are acquired before stream tasks are spawned, and each connection has a per-connection stream semaphore.
- TinyUFO L1: the default follows `min(512 MiB, cache_budget_bytes)` on the janitor cycle; explicit `memoryCapacity` remains explicit. Budget hysteresis compares against the applied capacity so gradual changes eventually resize.
- Adaptive Bloom: layer growth is bounded by `bloom_budget_bytes`.
- Negative cache: capacity is bounded by Bloom scale and governor negative-cache budget.
- Cache purger, janitor, warmup, and L2 promotion: use `BackgroundWork` admission and skip under pressure.

## Large-Scale Behavior

For very large caches, Bloom starts small and grows in sharded layers instead of preallocating for worst-case key counts. At billion-key scale it keeps reads concurrent, grows without rebuilding existing layers, and stops adding layers when the Bloom budget is reached. Negative cache remains short lived and bounded; it cannot grow independently of the governor.

The old fixed caps are replaced by dynamic limits. Remaining numeric constants are guardrails, not single-node scale caps.

## L4 Adaptive Defense

Automatic L4 blocking reuses the existing control-plane `emptyConnectionFlood` policy. When `emptyConnectionFlood.isOn=true` and `maxEmptyConnections>0`, TCP, UDP, SNI passthrough, QUIC passthrough, and HTTP/3 resource-exhaustion events can trigger local cluster-scope blocks and `ip_report` synchronization. When the policy is off, the node still applies local admission, drop, timeout, and metrics, but it does not report automatic blacklist entries.

The L4 pressure level is computed as `Normal`, `Elevated`, `High`, or `Critical` from:

- connection admission usage.
- memory pressure.
- FD-equivalent pressure.
- TCP/HTTP/H3/UDP admission rejects.
- UDP queued byte usage.
- QUIC route, pending route, and reassembly usage.
- IPv4 `/24`, IPv6 `/64`, and cluster-wide distinct-IP surge.

Per-IP TCP-like active connection limits are shared by HTTP, HTTPS, TCP, TCP-TLS, and SNI passthrough. They start broad in Normal pressure and tighten as pressure rises. Slow-first-byte, slow ClientHello, and slow HTTP header deadlines also tighten under pressure so distributed slow attacks release FDs and tasks earlier.

L4 event thresholds are still based on `emptyConnectionFlood.maxEmptyConnections`. High-confidence events use the base threshold, medium-confidence events use `2x`, and low-confidence events use `4x`; under High and Critical pressure the effective threshold is reduced to one half or one quarter with a floor of 2. Source failures, origin timeouts, backend resets, normal SNI passthrough, and valid QUIC/HY2 passthrough are not counted as attacks.

Exact IP counters are kept in a reserved L4 counter table so high-confidence L4 events do not fail open if the general WAF rolling counter table is full. Prefix scoring is used mainly to increase pressure and trigger local emergency drop. A temporary cluster-scope prefix block is allowed only for sustained high-confidence events and only when no whitelist covers that prefix.

Inbound PROXY Protocol is trusted only from loopback, private, or link-local immediate peers. Public peers may send a syntactically valid PROXY header, but it is consumed without replacing the real socket client IP, so L4 defense, WAF counters, access logs, and blacklist reports cannot be spoofed by the header.

## Pingora Cache Notes

`openFileCache` controls whether small L2 disk HITs may be read into memory for faster serving and L1 promotion. When it is disabled, small L2 HITs stream from the file path instead of being pulled into a `Bytes` buffer. The memory read requests exactly the charged object length (`try_reserve_exact`, then a boxed slice so reported capacity equals that length). This is Rust allocation capacity, not allocator usable size or RSS. Both the shared-account charge and the cache-read ledger charge transfer to the `Bytes` owner: they are refunded when the last downstream slice is released, not when the handler drops. Compressed decode moves its permit into the blocking task, including normalization and body handoff, so cancelling the lookup cannot refund a still-running decode. The compressed-file portion stays with the returned handler; the decompressed body carries its own charge. The decoder workspace and output growth transient are covered by the 4 MiB admission estimate, rather than the object-length ledger. Streaming compressed hits do not yet hold an equivalent decoder-workspace permit; that remains an accounting gap.

`enableSendfile` is tracked and exposed in runtime stats. The current Pingora cache `HandleHit` interface returns `Bytes` chunks and supports seek, but it does not expose the downstream socket to storage handlers. Because of that, this node cannot safely call Linux `sendfile(2)` inside the existing storage handler. The current implementation uses larger disk HIT chunks when sendfile is requested and implements Pingora `seek` for memory HIT handlers so range responses avoid extra filtering and over-read. True kernel sendfile would require a larger Pingora serving-path extension or a custom response path that owns both the file descriptor and downstream connection.

## Pressure Detection and Reclaim

Pressure classification still comes from the governor snapshot (thresholds
unchanged), but detection no longer relies on the 2s snapshot TTL alone. On
Linux, `start_reclaim_monitor` also starts an idempotent event watcher that
polls two kernel sources and, on wakeup, invalidates the snapshot cache so the
next snapshot re-reads cgroup state immediately:

- PSI `/proc/pressure/memory` trigger `some 200000 1000000` (any-task memory
  stall ≥200ms per 1s window). Works on cgroup v1 and v2.
- cgroup v2 `memory.events`, resolved by walking the process cgroup upward
  until a readable file is found (delegated hierarchies often only expose it
  on an ancestor). Rising `high` counters floor the observation at `High`;
  `max`, `oom`, or `oom_kill` increments floor it at `Critical`. Events are
  coalesced to 250ms and never lower the pressure level — the snapshot-derived
  classifier remains the single source of truth.
- The classifier additionally folds in PSI avg10 stall ratios (oomd-style):
  `some` ≥10% floors the level at `Elevated`; `some` ≥30% or `full` ≥5%
  floors it at `High`. Ratio thresholds measure how much memory is *left*;
  PSI measures how much tasks are *already stalling* — a host in reclaim
  congestion can thrash while still reporting free bytes. PSI floors only
  ever escalate; `Critical` stays byte-threshold/event-driven so a transient
  stall cannot trigger the most destructive reclaim. Both ratios are exported
  as `psiMemorySomeAvg10PctX100`/`psiMemoryFullAvg10PctX100`.

Reclaim actions target the allocator that actually owns the heap. The global
allocator is mimalloc, so heap return uses `mi_collect` instead of the
glibc-only `malloc_trim`:

- `Elevated`: non-forced `mi_collect(false)` on a rotating thread basis via the
  60s cache-janitor cycle (the janitor task migrates across Tokio workers, so
  repeated cycles eventually collect each worker's heap; `mi_collect` only
  affects the calling thread).
- L1 entry weight now accounts for the entry's real heap cost — body bytes
  plus the cache key, response header name/value bytes (names are stored
  twice, raw + case-preserved), and a fixed ~512B struct/index/bookkeeping
  overhead. Previously only `data.len()` was weighed, so header-heavy
  entries could hold several times the nominal byte budget.
- `High`/`Critical`: forced `mi_collect(true)` in the reclaim monitor.
- After `ConfigStore::replace_all_servers` under High/Critical: forced collect
  on the same thread that dropped the previous generation, so its segments
  are purged before reuse.
- moka-backed caches (L1, regex cache, WAF regex cache) call
  `run_pending_tasks()` after `invalidate_all()` so evictions actually release
  memory instead of waiting for moka's lazy janitor.

`ReclaimStats` records process RSS before and after each reclaim pass, so the
effectiveness of a reclaim cycle is observable instead of assumed.

### Reclaim robustness (unwind builds)

The reclaim machinery is panic-hardened so a single faulting pass cannot
silently disable the whole subsystem:

- `RECLAIM_IN_FLIGHT`, the thread-local `RECLAIM_IN_PROGRESS`, and the
  ledger's `RESIDENT_OWNER_UPDATE_IN_PROGRESS` are all cleared by RAII guards
  during unwinding, so a panic can never wedge a flag permanently.
- `request_reclaim` contains unwind panics with `catch_unwind`: the pass is
  logged as an error, the panic still hits the default panic hook, and the
  call returns `None` so the coordinator rolls the trigger back and the
  pressure level is re-observed on the next cycle instead of being lost. The
  cooldown is armed even on panic so a deterministically faulting path cannot
  hot-loop.
- The reclaim-monitor loop and the kernel pressure-event watcher each wrap
  their iteration body in `catch_unwind`, so neither dedicated thread dies on
  a faulting iteration — the pending-level slot and unpark target keep working.
- `start_reclaim_monitor` is idempotent (`RECLAIM_MONITOR_STARTED` guard);
  a repeated call can no longer spawn a duplicate monitor thread that would
  drain the same pending level. A failed spawn clears the flag so a later
  call can retry.
- The cooldown's `last == 0` state means "never reclaimed", so the first
  reclaim of a fresh process is not suppressed, and the cooldown is measured
  from reclaim completion rather than reclaim start. Release builds use
  `panic = "abort"`, where containment is moot; these guards protect debug
  builds and tests.

### Bounded emergency shed (`memory_shed`)

Cooperative reclaim cannot touch work already in flight — a long download or
lingering keepalive connection holds its buffers until it finishes, which can
lose the race against the OOM killer at `Critical`. `observe_pressure` is
called from `on_memory_pressure_observed` on every pressure sample:

- `High` → keepalive suppression: `early_request_filter` writes responses
  with `set_keepalive(None)`, so idle connections drain at the next request
  boundary instead of holding buffers open. Cost when inactive: one relaxed
  atomic load per request.
- `Critical` → keepalive suppression plus a **final-resort** drain, only when
  all of these hold: Critical has been observed continuously for at least 10s
  with samples at least 5s apart; the latest real reclaim pass ran within 30s
  and lowered measured RSS by less than 4 MiB (or freed no entries/estimated
  bytes when RSS is unavailable); and a freshly read governor snapshot reports
  `account_headroom_bytes` as `Some` and strictly below
  `connectionAdmissionUsedBytes + requestWorkspaceUsedBytes`. The snapshot must
  also confirm Critical — PSI-only or stale Critical evidence cannot kill.
  Unknown headroom never kills. A pass cancels at most 32 oldest L4
  connections and 32 oldest H3 connections, skips records already being
  cancelled, consumes the ineffective-reclaim evidence, and is limited to one
  pass per 30s. Any sample below Critical ends the sustained episode; any
  effective reclaim, `High` sample, or stable recovery clears the evidence.
- Sub-`High` levels only maintain the 30s recovery window; suppression lifts
  after the observed level stays below `High` for that window.

New-connection refusal is deliberately not added: admission already
collapses toward its critical floor via `pressure_adjusted_min_limit`, and a
hard-refused connection produces no response at all. Shed activity is exported
on the node-status `resourceGovernor.shed` object (`drainedConnectionsTotal`,
`keepaliveMarkedTotal`, `criticalSamples`, `reclaimExhausted`,
`finalResortPassesTotal`, `lastFinalResortAtMs`, `keepaliveShedActive`,
`h3DrainedConnectionsTotal`).

### Protocol-level refusal semantics

Resource shortage for a new HTTP/2 stream resets only that stream with
`REFUSED_STREAM` and keeps the accept loop polling the connection, so accepted
sibling streams continue. The former behavior exited the connection on a
per-connection or global stream admission failure; that turned ordinary memory
shortage into loss of already accepted work. Sustained creation abuse still
triggers the existing 200-stream/30s defense, but it now sends GOAWAY through
`graceful_shutdown` and keeps the connection polled until accepted responses
finish. H3 request admission failure resolves the request through the public h3
API and rejects both stream halves with `H3_REQUEST_REJECTED`; dropping a bare
resolver was not used because it skips h3's request-completion bookkeeping and
leaks the request id. Neither protocol's new-work refusal closes the connection.

### Per-connection admission tickets (`memory_ticket`)

Request-scoped byte classes (`RequestBodyWaf`, `ResponseBodyWaf`,
`ResponseTransform`) previously ran a global padded-counter RMW per
admission — ~4 shared-line ops per request. Each registered L4 connection
now owns a `TicketBucket` (indexed by peer socket address in
`L4ConnectionRegistry::by_addr`, looked up once per request in
`early_request_filter`). QUIC has no stable peer-address key, so each
accepted H3 connection creates one bucket and scopes it to every request task;
address indexing is never used for QUIC. A bucket refills from the
`request_workspace` ledger in `need + 256KiB` chunks; spends are an
uncontended CAS on a connection-private line.

- Ledger bound: `available/4` normally, `available/16` under High+
  pressure, floored at one transform charge. Classes on one connection share
  the pool.
- Owner bound: one bucket may hold at most `max(workspace/4, 16MiB)`,
  capped at the whole workspace. The 16 MiB floor lets one legitimate
  transform fit; pools below 4×16 MiB can still be consumed by that single
  transform, so the guarantee is proportional rather than absolute. Refills
  reserve owner bytes with an atomic before touching the global ledger, so
  concurrent refills cannot each observe the same unused allowance. A victim
  owner can still obtain a 16 KiB charge while another owner is capped.
- H2 stream slots are **not** drawn from this byte pool. Pooled and
  unregistered H2 paths both hold an ordinary `Http2Stream` class permit for
  exactly the stream lifetime, so the global stream count and shared
  connection estimate cannot be bypassed. No second workspace byte charge is
  taken for a stream slot.
- `release` returns bytes to the bucket and refunds the global ledger once
  the idle balance exceeds `1MiB`, so a 16MiB transform charge is not held
  for the connection's lifetime; bucket drop (connection close) refunds the
  rest. Spend failure returns `None` and bumps the class reject counter —
  identical fail-closed contract to `try_admit`.
- Unregistered non-H2 paths take the `WorkspacePermit::Direct` fallback —
  `try_admit` semantics unchanged.
- Microbench (debug, 8×2M, same mixed-class rotation): pooled 3.99M ops/s
  vs direct 2.83M ops/s (+41%). This predates the owner bound and counted
  H2 slots as byte spends; it is not a measurement of the current stream path.
- `requestWorkspaceUsedBytes` is exported on node status and folded into
  the `unaccountedRssBytes` tracked set like the other estimate ledgers.

### Bounded scoped-IP state (`firewall::bounded_map`)

The seven attacker-driven scoped-IP maps (`blocks`, `kernel_blocks`,
`list_blocks`, `whitelists`, `list_whitelists`, `graylists`,
`list_graylists`) moved from `DashMap` to `BoundedScopedMap`: physical
capacity is allocated once at construction from the governor's state
capacity, so an IP flood cannot grow the table past its budget. 64 mutexed
segments × a fixed 8-slot probe window make insert/update/lookup O(PROBES)
with no global scan; hashing uses a per-instance `ahash::RandomState` so
precomputed-collision floods do not apply.

At the soft cap, an insert evicts the earliest-expiry entry *inside the
sampled probe window* (with a rare cross-segment fallback scan when the
window holds no live victim). Eviction is therefore **sampled, not global
earliest-expiry** — a deliberate trade for constant-time inserts under
sustained floods. Expired entries are preferred for reuse inside the window,
and the periodic GC/`retain` pass still reclaims expired rows exactly.
Lookup, update, removal, expiry, persistence, RCU union snapshots, and
eviction metrics are unchanged; network/range maps stay `DashMap` (they are
bounded by config cardinality, not attacker-floodable).

## Resident Ledger Consistency

Every map that charges the resident ledger refunds its owner on every removal
path: per-key delete, expiry cleanup, capacity eviction, and bulk drain.
Negative-cache inserts charge the ledger *before* writing the entry, so a
rejected charge can never leave an uncharged entry behind (the rejection
increments `negativeCacheAdmissionRejected`). The same charge-before-insert
rule applies to the cache access log (`CACHE_ACCESS_LOG` uses the `Entry`
API so a rejected charge skips the insert) and to surrogate index members
(a rejected member charge degrades the tag to a saturated mark, which purge
handles via the authoritative metadata scan).

Attacker-driven maps are hard-bounded regardless of TTL sweeping:
`HTTP_REQUEST_PARSE_MARKS` (client-address keyed) caps at 262k entries
(65k under High+ pressure) with a force-sweep plus a rate-limited warning;
a dropped mark only means the connection is treated as having no parsed
request — the fail-closed direction for the L4 early-close signal.

Three mechanisms bound residual drift:

- Saturated surrogate marks are bounded by `surrogate_index_capacity()` and
  ledger-charged like index members, so origin-controlled `Surrogate-Key`
  headers cannot grow them without limit.
- `reclaim_caches_critical` additionally drops the whole surrogate reverse
  index (`surrogateIndexTagsRemoved` in `lastReclaim`). Purge stays correct
  because the purge path always merges the authoritative metadata scan; the
  index is only an accelerator.
- Every 5 minutes the reclaim monitor runs a ledger-vs-map reconciliation
  (skipped above Elevated pressure) that refunds owners whose key no longer
  exists. Cumulative `ledgerReconcileStaleOwners`/`ledgerReconcileBytesRefunded`
  are exported in `resourceGovernor`; a nonzero counter indicates a missed
  refund somewhere and should be investigated rather than accepted.
- The ledger's own owner map is bounded (`RESIDENT_LEDGER_MAX_OWNERS`):
  brand-new owner charges are refused at the cap — every call site already
  treats charge failure as fail-closed — while updates/refunds of tracked
  owners always proceed so accounting stays exact. Refusals are exported as
  `ledgerOwnerCapRejections` and current rows as `ledgerTrackedOwners`.
- High-pressure reclaim also trims the surrogate reverse index down to
  `SURROGATE_INDEX_MAX_TAGS_PRESSURE`, dropping the largest membership sets
  first and refunding every member owner; saturated marks are retained
  because they are cheap and keep degraded-purge semantics.

Other formerly insert-only maps are now bounded or self-cleaning:
`ORIGIN_HEALTH_MAP` (stale-check GC in the reporter plus a 65k backstop),
`APPLIED_PURGE_IDS` (65k cap; beyond it dedup degrades to re-applying an
idempotent purge), `REPLICA_STATS` (the 30s staleness filter now also
removes dead rows), and `HEADER_NAME_CACHE` (65k).

## WAF Scoped-State and Persistence Bounds

`WafStateManager` scoped maps (`blocks`, `kernel_blocks`, `block_networks`,
`kernel_block_networks`, `list_blocks`, `list_whitelists`, `graylists`, and
their network/range mirrors) are attacker-driven — a spoofed-source flood can
mint one entry per IP. All scoped insertions now run through
`apply_scoped_ip_with_capacity`-style paths bounded by
`governor.firewall_state_map_capacity()` (pressure-aware: ~1M normal,
~131k under pressure). When a map is full, expired entries are swept first,
then earliest-`expires_at` live entries are evicted in batches
(`over.max(capacity/16)`, clamped to 262144) — approved policy:
evict-soonest-expiring rather than refuse the new block. The batch scales
with capacity because each victim scan is O(map): a fixed small batch would
rescan the whole map every few hundred inserts under a sustained storm. Evictions increment `wafStateEvicted`,
emit a rate-limited warning, and reconcile paired kernel-mirror maps plus
range/network snapshots so enforcement stays coherent. Existing-key updates
never evict. Approved tradeoff: under extreme cardinality pressure the
soonest-expiring blocks can be released early; the new block always takes
effect.

`firewall::persistence::PENDING` (coalesced upserts + delete tombstones held
while storage is unavailable) is bounded at
`firewall_state_map_capacity() / 4`, clamped to [4096, 2M]. Full upserts
evict earliest-expiring records; full tombstone sets drop arbitrary excess
deletes. Drops increment `firewallPendingDropped` and warn rate-limited.
Approved tradeoff: a dropped tombstone can let a deleted block resurrect on
restart (expiry cleanup is the backstop); a dropped upsert loses persistence
for that record while in-memory enforcement remains.

## Metrics Memory Observability

Metrics tracker memory (metric aggregators, top-IP tracker, daily-domain and
unique-IP trackers) is estimated every 30s and published as
`metrics_aggregator_bytes` on the governor snapshot. The existing periodic gauge
uses a coarse per-sample estimate and does not include dynamic attributes. The
on-demand `MetricAggregator::memory_estimate` API additionally estimates
attributes and flush vectors; it is not published on node status and may hold
shard read locks while walking samples. Shared allocations may be counted more
than once, and flushed batches and allocator slack are not measured. Neither
estimate is RSS or an enforced aggregate budget. These maps are not charged into
the resident ledger, which is deliberately scoped to cache categories. A
rate-limited `METRICS_MEMORY` warning fires when the gauge exceeds max(1/8 of
node memory, 64MiB).

Cache reads, writes, revalidation and allocation-heavy cache background work
borrow through the shared-account CAS gate: observed headroom minus safety
margin, outstanding commitments, the full connection budget and the control-plane
reserve. Borrowing closes outside Normal pressure and without observed physical
headroom; credential growth keeps the same priority. A cache-read admission
charges both the dedicated cache-read budget and the shared account, and a body
that outlives its handler keeps both charges until the last slice is dropped.

Ordinary shared grants cannot consume the control-plane reserve. That reserve is
the existing Normal-pressure minimum for 32 cluster-control connections plus 16
RPC stream commands, plus one configuration materialization. `ConfigApply` has
its own admission class and one slot, so cache background saturation cannot fill
the shared `BackgroundWork` class and block configuration. Expiry cleanup and
disk purging keep ordinary `BackgroundWork` admission because cleanup must still
progress under pressure. Configuration decode is bounded by the decode budget
and runs before the materialization permit; the materialization build window is
charged through `SharedGrantPurpose::ConfigPublish` at the decoded JSON length —
the cheapest measured lower bound of the maps being built, while the previous
generation is still installed. A refused charge keeps the previous generation.
The installed generation's steady-state residency is still not charged.

New connection establishment and established traffic no longer share one
undifferentiated budget. HTTP/TCP/H3 connection and UDP session admissions stop
at `shared connection budget − established reserve`. Only work that belongs to
an already accepted request may draw the reserve: origin connects and the
relay/session queue bytes (`TcpQueue`, `UdpQueue`, `ZeroCopyRelay`). New H2
streams, new H3 requests, request-body/response-body WAF workspaces and response
transforms use Ordinary account priority and stop at the same boundary, so new
work cannot squeeze the bytes an accepted response needs to keep forwarding.
The reserve is the Normal-pressure floors: 1,024 H2 streams × 16 KiB, 1,024 H3
requests × 24 KiB and 512 origin connects × 32 KiB (56 MiB), capped at half the
connection budget when the budget is below twice that. The same reserve is
deducted from the shared account for connection-establishment and Ordinary
admissions; only the Established continuation priority keeps just the
control-plane reserve. SNI relay and response transforms are excluded: SNI relay
is a legacy passthrough with its own half-budget limit, and a 16 MiB transform
estimate would make its floor a memory plan by itself.

`resourceGovernor.sharedAccount` reports `lowPriorityBorrowingEnforced=true`,
current `lowPriorityBorrowableBytes`,
`controlPlaneAccountReserveEnforced=true`, `controlPlaneReserveBytes`,
`establishedTrafficAccountReserveEnforced=true`, `establishedTrafficReserveBytes`
and `configApplyActive`. `priorityReserveEnforced=true` now means the
continuation reserve is reachable only through Established priority — origin
connects and relay/session queues — while new connections, streams, H3 requests
and large request workspaces stop before it. The 56 MiB remains a shared pool,
not a per-connection entitlement; a connection that has no accepted request
cannot borrow it. Remaining gaps stay explicit: the installed configuration
generation's steady-state residency is not charged, and the client-agent
verified-IP table has no bounded index yet. Pressure reclaim already revokes the
reclaimable L1/analytics caches — L1 is halved at High and drained at Critical,
with negative cache, Bloom, surrogate, geo/UA, TLS-connector and regex caches
trimmed alongside — while metric aggregation samples are billing state and are
never revoked (only new keys are cap-rejected). Logical refunds never increase
observed physical headroom, and estimated bytes are not RSS.

Per-server metric state now has a concrete deletion lifecycle. A configuration
snapshot that drops a server id moves its metric instance to a retired set; the
persistence, bandwidth and daily delta consumers each acknowledge the final
fingerprinted snapshot, and the entry is released only when all three have done
so and no request still holds the metrics Arc. Entries are keyed by a
process-local instance id so re-adding the same server id cannot mix baselines.
The retired set is capped at 4,096 instances; over-cap ids remain live and are
retried on a later configuration snapshot rather than dropping billing data.
Daily acknowledgement means the final delta entered the in-memory daily window;
remote/durable delivery keeps its existing window and spool semantics.

Tracker cardinality is additionally hard-bounded: each tracker
(`MetricAggregator` x2 incl. nested `request_samples`, `TopIpTracker`,
`DailyDomainTracker`, `UniqueIpTracker`) admits new keys only while below
`governor.metrics_cardinality_capacity()` — `state_budget_bytes / 40`
divided by a 256-byte entry estimate, clamped between
`MIN_METRIC_CARDINALITY_ENTRIES` and `MAX_METRIC_CARDINALITY_ENTRIES`, so the
limit scales with node memory and shrinks under pressure. New keys beyond the
cap are dropped with `metricsCardinalityDropped` incremented and a
rate-limited warning; existing keys always keep accumulating so tracked
totals stay exact. `restore`/seed-load paths honor the same cap — persisted
state cannot bypass it. Dropped unique keys are also not persisted, so the
observed cardinality floor equals the cap.

Mace storage capacity is selected cgroup-first: `memory.max`, then
`memory.high`, then host total. A small cgroup limit can no longer pick a
host-sized storage tier.

## Hot-Path Contention (Derived Ledgers + RCU WAF Reads)

Admission uses per-class counters on **dedicated cache lines**
(`PaddedAtomicU64`) with an add→check→rollback protocol — one `fetch_add`
plus one `load`, no CAS retry loop, and floods on different classes never
false-share a line. The counter may transiently read
`limit + in-flight adders`, but a grant is issued only while the settled
count is below the limit, and every overflow path rolls its add back.
The shared connection byte ledger is **derived**, not stored:
`Σ(count_class × per-class charge)` over the eight connection-level classes
is recomputed on read — the seven quiet class lines stay Shared in local
cache, so the read costs only the flooded line's ownership transfer. This
removes two shared-cacheline RMWs from every connection admission compared
with a real ledger.

All admission budgets/limits are **materialized**, not recomputed per op:
`GovernorLimits` (17 class limits + shared/cache-read/zero-copy/queue
budgets) is built once per generation and published through `ArcSwap`. A
`cached_generation` counter is bumped on every write to the cached memory
inputs (snapshot refresh, invalidation, test reseeds); the hot path loads
one `Arc` and reads fields — zero snapshot reads, zero division/clamp
recomputation. `limits()` also re-checks the snapshot TTL clock on every
call, because admit paths no longer call `memory_snapshot()` — without
that check nothing would re-arm the periodic refresh and pressure
classification would freeze. Stale generations/TTL trigger an idempotent
rebuild (concurrent
rebuilds race, last store wins; the generation is sampled before the
snapshot read so a mid-refresh input write invalidates the result rather
than publishing a mixed-input table). A test pins the table to always
equal the live `limit_for` computation.
`cache_read_memory_bytes` and `zero_copy_relay_bytes` remain plain
`fetch_add` byte ledgers (one padded line each); `udp_queued_bytes`/
`tcp_queue_bytes` keep strict CAS because their never-exceed-semantics
contract is load-bearing for queue budgets.

A 64-way cacheline-sharded counter (`ShardedU64`) was implemented and
measured on this workload — it was **reverted** because every admission
needs the aggregate total, and summing 64 remote cachelines per operation
(~2.9× slower than the shared-CAS baseline in the 8-thread×2M admission
bench) costs more than the write contention it avoids. Per-CPU-style
counters only pay off when totals are aggregated out-of-band, not per op.

WAF scoped-IP reads (`is_blocked`/`is_whitelisted`/`is_graylisted`) go
through an RCU read path: an immutable per-kind union snapshot
(`blocks∪list_blocks`, `whitelists∪list_whitelists`, `graylists∪
list_graylists`) published via `ArcSwap`, plus a small delta map carrying
every mutation since the last snapshot build. Writers still mutate the
source DashMaps (the authority), then publish the resolved union value into
the delta; when the delta reaches `IP_DELTA_CAP` (262144) a single-flight
rebuild installs a fresh delta, folds the source maps into a new snapshot,
and swaps it in. A SeqCst fence on both sides makes a lost write impossible:
either the snapshot scan observes the map mutation, or the delta insert
lands in the post-swap delta. Delta entries carrying `IP_DELTA_REMOVED`
shadow stale snapshot entries, so evicted/removed keys disappear instantly.
Reads are instant-consistent (no enforcement gap) and wait-free — no shard
locks over the multi-million-entry maps.

`unaccountedRssBytes` on the node status resourceGovernor section reports
`process RSS − (resident ledger + connection/relay/queue/cache-read ledgers
+ metrics gauge)`, saturating at 0. It is an attribution-gap signal, not
leak proof — allocator slack and uninstrumented structures legitimately
count there; sustained growth means estimator drift or missing ledgers.

The node pressure signal propagated over `X-Cloud-Node-Pressure` now includes
a memory component (elevated memory pressure raises the score alongside the
existing connection/CPU mix), so L1/L2 peers see memory stress instead of an
idle-looking node.

## API Compatibility

The node does not require new control-plane configuration fields for this feature. Control-plane-visible memory totals use the same governor snapshot as admission decisions, including cgroup working-set accounting. Runtime visibility is local through logs, `memory_plan`, admission reject counters, and local performance samples. Node status also includes best-effort `resourceGovernor` and `l4Defense` JSON sections with FD pressure, zero-copy permits, UDP queued bytes, L4 pressure, top event kind, top prefix, prefix pressure, aggregate drops, and exact-counter saturation. This keeps configuration compatibility while allowing newer control planes to display the richer runtime snapshot.
