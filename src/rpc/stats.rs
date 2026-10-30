use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::metrics::{MetricConsumer, ServerMetrics, ServerStatusSnapshot};
use crate::pb;
use crate::rpc::client::SharedRpcClient;
use crate::rpc::metric_stat_cache::{
    DirtySnapshotRow, MetricStatReporterState, MetricValuesCacheStats, SnapshotKey,
};
use chrono::{DateTime, Duration as ChronoDuration, FixedOffset, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use tracing::{debug, info, warn};

const ORIGIN_HEALTH_STATUS_HEALTHY: u8 = 1;
const ORIGIN_HEALTH_STATUS_DOWN: u8 = 2;

struct OriginHealthEntry {
    status: AtomicU8,
    latency_ms: AtomicI64,
    last_check_ts: AtomicI64,
}

static ORIGIN_HEALTH_MAP: Lazy<dashmap::DashMap<i64, Arc<OriginHealthEntry>>> =
    Lazy::new(dashmap::DashMap::new);

/// Entries for origins removed from the configuration stop receiving health
/// events; the reporter GCs them once their last check ages past this TTL.
const ORIGIN_HEALTH_STALE_SECS: i64 = 1800;
/// Backstop bound on tracked origins — far above any legitimate origin count.
const ORIGIN_HEALTH_MAX_ENTRIES: usize = 65_536;

pub fn push_origin_health_event(origin_id: i64, healthy: bool, latency_ms: i64) {
    push_origin_health_event_with_capacity(
        origin_id,
        healthy,
        latency_ms,
        ORIGIN_HEALTH_MAX_ENTRIES,
    );
}

fn push_origin_health_event_with_capacity(
    origin_id: i64,
    healthy: bool,
    latency_ms: i64,
    max_entries: usize,
) {
    if origin_id <= 0 {
        return;
    }
    let now = crate::utils::time::now_timestamp();
    let status = if healthy {
        ORIGIN_HEALTH_STATUS_HEALTHY
    } else {
        ORIGIN_HEALTH_STATUS_DOWN
    };
    if let Some(entry) = ORIGIN_HEALTH_MAP.get(&origin_id) {
        entry.status.store(status, Ordering::Relaxed);
        entry.latency_ms.store(latency_ms, Ordering::Relaxed);
        entry.last_check_ts.store(now, Ordering::Relaxed);
    } else {
        if ORIGIN_HEALTH_MAP.len() >= max_entries {
            static ORIGIN_HEALTH_FULL_WARN: AtomicI64 = AtomicI64::new(0);
            let last = ORIGIN_HEALTH_FULL_WARN.load(Ordering::Relaxed);
            if now.saturating_sub(last) >= 600
                && ORIGIN_HEALTH_FULL_WARN
                    .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                warn!(
                    entries = ORIGIN_HEALTH_MAP.len(),
                    "origin health map at capacity — new origin ids are not tracked until stale entries expire"
                );
            }
            return;
        }
        ORIGIN_HEALTH_MAP.insert(
            origin_id,
            Arc::new(OriginHealthEntry {
                status: AtomicU8::new(status),
                latency_ms: AtomicI64::new(latency_ms),
                last_check_ts: AtomicI64::new(now),
            }),
        );
    }
}

fn gc_stale_origin_health(now_ts: i64) -> usize {
    let before = ORIGIN_HEALTH_MAP.len();
    ORIGIN_HEALTH_MAP.retain(|_, entry| {
        now_ts.saturating_sub(entry.last_check_ts.load(Ordering::Relaxed))
            <= ORIGIN_HEALTH_STALE_SECS
    });
    before.saturating_sub(ORIGIN_HEALTH_MAP.len())
}

pub async fn start_origin_health_reporter(api_config: ApiConfig) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        // Drop entries whose origins stopped producing health events — an
        // actively checked origin refreshes last_check_ts every cycle, so
        // only removed/disabled origins age out. Runs even when not leader
        // so a long follower stint cannot strand stale entries.
        gc_stale_origin_health(crate::utils::time::now_timestamp());
        if ORIGIN_HEALTH_MAP.is_empty() {
            continue;
        }
        let created_at = crate::utils::time::now_timestamp();
        let items: Vec<_> = ORIGIN_HEALTH_MAP
            .iter()
            .map(|entry| {
                let origin_id = *entry.key();
                let e = entry.value();
                let status_str = match e.status.load(Ordering::Relaxed) {
                    ORIGIN_HEALTH_STATUS_HEALTHY => "healthy",
                    ORIGIN_HEALTH_STATUS_DOWN => "down",
                    _ => "unknown",
                };
                pb::create_node_values_request::NodeValueItem {
                    item: format!("originHealth_{}", origin_id),
                    value_json: serde_json::json!({
                        "originId": origin_id,
                        "status": status_str,
                        "latencyMs": e.latency_ms.load(Ordering::Relaxed),
                        "lastCheckTs": e.last_check_ts.load(Ordering::Relaxed),
                    })
                    .to_string()
                    .into_bytes(),
                    created_at,
                }
            })
            .collect();

        if items.is_empty() {
            continue;
        }

        let client = match SharedRpcClient::get(&api_config).await {
            Ok(shared) => shared.as_rpc_client(),
            Err(e) => {
                warn!("Origin health reporter failed to connect: {}", e);
                continue;
            }
        };
        let mut service = client.node_value_service();
        if let Err(e) =
            crate::rpc::track_rpc(service.create_node_values(pb::CreateNodeValuesRequest {
                node_value_items: items,
            }))
            .await
        {
            debug!("Origin health report failed: {}", e);
        }
    }
}

/// Billable volume is downstream egress only. Inbound request bytes are
/// reported separately (`bytes_in`) so the control plane can apply the
/// per-region billing direction at settlement time — the node never bakes
/// the billing policy into its counters.
fn billable_bytes(snapshot: &ServerStatusSnapshot) -> u64 {
    snapshot.bytes_sent
}

const BANDWIDTH_REPORT_INTERVAL_SECS: u64 = 300;
const BANDWIDTH_SAMPLE_INTERVAL_SECS: u64 = 2;
const BANDWIDTH_ALGO_AVG: &str = "avg";

fn bandwidth_bytes_per_sec(stat: &BandwidthWindowStat, config_store: &ConfigStore) -> u64 {
    let use_avg = stat.user_plan_id > 0
        && config_store
            .get_user_plan_sync(stat.user_plan_id)
            .and_then(|user_plan| user_plan.user)
            .map(|user| user.bandwidth_algo == BANDWIDTH_ALGO_AVG)
            .unwrap_or(false);

    if use_avg {
        stat.total_bytes / BANDWIDTH_REPORT_INTERVAL_SECS
    } else {
        stat.peak_bytes_per_sec
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct BandwidthWindowStat {
    day: String,
    time_at: String,
    user_id: i64,
    server_id: i64,
    user_plan_id: i64,
    #[serde(default)]
    count_errors: u64,
    peak_bytes_per_sec: u64,
    total_bytes: u64,
    bytes_in: u64,
    cached_bytes: u64,
    attack_bytes: u64,
    count_requests: u64,
    count_cached_requests: u64,
    count_attack_requests: u64,
    count_websocket_connections: u64,
    origin_total_bytes: u64,
    origin_avg_bytes: u64,
    origin_avg_bits: u64,
    count_ips: u64,
}

/// A fully accumulated window that could not enter the durable queue yet.
/// The data is complete — it is never a gap; it is re-offered oldest first.
struct PendingWindow {
    window_start: i64,
    segment_started_at: i64,
    source_boot: String,
    stats: Vec<pb::ServerBandwidthStat>,
    error_counts: HashMap<i64, u64>,
}

#[derive(Default, Serialize, Deserialize)]
struct StoredBandwidthState {
    current_window: String,
    window_stats: Vec<BandwidthWindowStat>,
    #[serde(default)]
    pending_windows: Vec<StoredPendingWindow>,
    #[serde(default)]
    boot: String,
    #[serde(default)]
    segment_started_at: i64,
    // Legacy field kept only for decode compatibility with pre-spool states.
    #[serde(default)]
    pending_stats: Vec<StoredPendingBandwidthStat>,
}

/// A fully accumulated window that could not enter the durable queue yet.
/// The data is complete — it is never a gap; it is re-offered oldest first.
#[derive(Clone, Default, Serialize, Deserialize)]
struct StoredPendingWindow {
    window_start: i64,
    segment_started_at: i64,
    #[serde(default)]
    source_boot: String,
    stats: Vec<StoredPendingBandwidthStat>,
    #[serde(default)]
    error_counts: Vec<(i64, u64)>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct StoredPendingBandwidthStat {
    queued_at: i64,
    user_id: i64,
    server_id: i64,
    day: String,
    time_at: String,
    bytes: i64,
    bits: i64,
    total_bytes: i64,
    bytes_in: i64,
    cached_bytes: i64,
    attack_bytes: i64,
    count_requests: i64,
    count_cached_requests: i64,
    count_attack_requests: i64,
    user_plan_id: i64,
    count_websocket_connections: i64,
    origin_total_bytes: i64,
    origin_avg_bytes: i64,
    origin_avg_bits: i64,
    count_i_ps: i64,
    node_region_id: i64,
}

const BANDWIDTH_STATE_STORAGE_KEY: &str = "STAT_BANDWIDTH_STATE_V1";
/// Closed windows retained in heap while the durable spool refuses them.
/// Twelve windows is one hour of outage. Beyond it the reporter records a
/// counted coverage gap rather than growing without bound; the receiver keeps
/// the window incomplete, so settlement cannot treat it as full coverage.
const MAX_PENDING_BANDWIDTH_WINDOWS: usize = 12;

impl From<&pb::ServerBandwidthStat> for StoredPendingBandwidthStat {
    fn from(stat: &pb::ServerBandwidthStat) -> Self {
        Self {
            queued_at: 0,
            user_id: stat.user_id,
            server_id: stat.server_id,
            day: stat.day.clone(),
            time_at: stat.time_at.clone(),
            bytes: stat.bytes,
            bits: stat.bits,
            total_bytes: stat.total_bytes,
            bytes_in: stat.bytes_in,
            cached_bytes: stat.cached_bytes,
            attack_bytes: stat.attack_bytes,
            count_requests: stat.count_requests,
            count_cached_requests: stat.count_cached_requests,
            count_attack_requests: stat.count_attack_requests,
            user_plan_id: stat.user_plan_id,
            count_websocket_connections: stat.count_websocket_connections,
            origin_total_bytes: stat.origin_total_bytes,
            origin_avg_bytes: stat.origin_avg_bytes,
            origin_avg_bits: stat.origin_avg_bits,
            count_i_ps: stat.count_i_ps,
            node_region_id: stat.node_region_id,
        }
    }
}

impl From<StoredPendingBandwidthStat> for pb::ServerBandwidthStat {
    fn from(value: StoredPendingBandwidthStat) -> Self {
        Self {
            user_id: value.user_id,
            server_id: value.server_id,
            day: value.day,
            time_at: value.time_at,
            bytes: value.bytes,
            bits: value.bits,
            total_bytes: value.total_bytes,
            bytes_in: value.bytes_in,
            cached_bytes: value.cached_bytes,
            attack_bytes: value.attack_bytes,
            count_requests: value.count_requests,
            count_cached_requests: value.count_cached_requests,
            count_attack_requests: value.count_attack_requests,
            user_plan_id: value.user_plan_id,
            count_websocket_connections: value.count_websocket_connections,
            origin_total_bytes: value.origin_total_bytes,
            origin_avg_bytes: value.origin_avg_bytes,
            origin_avg_bits: value.origin_avg_bits,
            count_i_ps: value.count_i_ps,
            node_region_id: value.node_region_id,
            ..Default::default()
        }
    }
}

impl From<&PendingWindow> for StoredPendingWindow {
    fn from(value: &PendingWindow) -> Self {
        Self {
            window_start: value.window_start,
            segment_started_at: value.segment_started_at,
            source_boot: value.source_boot.clone(),
            stats: value
                .stats
                .iter()
                .map(StoredPendingBandwidthStat::from)
                .collect(),
            error_counts: value.error_counts.iter().map(|(k, v)| (*k, *v)).collect(),
        }
    }
}

impl From<StoredPendingWindow> for PendingWindow {
    fn from(value: StoredPendingWindow) -> Self {
        Self {
            window_start: value.window_start,
            segment_started_at: value.segment_started_at,
            source_boot: value.source_boot,
            stats: value
                .stats
                .into_iter()
                .map(pb::ServerBandwidthStat::from)
                .collect(),
            error_counts: value.error_counts.into_iter().collect(),
        }
    }
}

/// Window accumulation lives in the same durable Mace store as the upload
/// queue (`state:` keys) — never the legacy non-synced metric store.
fn load_bandwidth_state() -> (
    String,
    HashMap<i64, BandwidthWindowStat>,
    Vec<PendingWindow>,
    String,
    i64,
) {
    let empty = || (String::new(), HashMap::new(), Vec::new(), String::new(), 0);
    let Ok(Some(raw)) = crate::telemetry::load_state(BANDWIDTH_STATE_STORAGE_KEY) else {
        return empty();
    };
    let state: StoredBandwidthState = match serde_json::from_slice(&raw) {
        Ok(state) => state,
        Err(e) => {
            warn!("bandwidth window state decode failed: {}", e);
            return empty();
        }
    };

    // `window_stats` always describes `current_window` at save time. Do not
    // filter it by the clock here: after a restart that window is stale and
    // `flush_stale_window` must upload it as an incomplete segment. Filtering
    // by age would silently discard its billable rows before that handoff.
    let mut window_stats = HashMap::new();
    for stat in state.window_stats {
        window_stats.insert(stat.server_id, stat);
    }
    let current_window = if window_stats.is_empty() {
        String::new()
    } else {
        state.current_window
    };

    // Legacy `pending_stats` (pre-spool state) is dropped: its entries were
    // either already uploaded or their windows surface as incomplete.
    let mut pending_windows: Vec<PendingWindow> = state
        .pending_windows
        .into_iter()
        .map(PendingWindow::from)
        .collect();
    if pending_windows.len() > MAX_PENDING_BANDWIDTH_WINDOWS {
        // Restoring an unbounded backlog into heap would peak with the outage
        // length. Keep a contiguous oldest run and report the remainder as a
        // coverage gap; never present it as delivered or complete.
        let first_dropped = pending_windows[MAX_PENDING_BANDWIDTH_WINDOWS].window_start;
        warn!(
            retained = MAX_PENDING_BANDWIDTH_WINDOWS,
            dropped = pending_windows.len() - MAX_PENDING_BANDWIDTH_WINDOWS,
            first_dropped,
            "bandwidth state has more pending windows than the restore cap; recording a coverage gap"
        );
        crate::telemetry::record_gap(
            "bandwidth",
            first_dropped,
            "restore of bandwidth backlog exceeded the 12-window heap cap",
        );
        pending_windows.truncate(MAX_PENDING_BANDWIDTH_WINDOWS);
    }

    (
        current_window,
        window_stats,
        pending_windows,
        state.boot,
        state.segment_started_at,
    )
}

async fn persist_bandwidth_state(
    current_window: &str,
    window_stats: &HashMap<i64, BandwidthWindowStat>,
    pending_windows: &[PendingWindow],
    segment_started_at: i64,
) {
    match encode_bandwidth_state(
        current_window,
        window_stats,
        pending_windows,
        segment_started_at,
    ) {
        Ok(raw) => {
            if let Err(e) = crate::telemetry::save_state(BANDWIDTH_STATE_STORAGE_KEY, raw).await {
                warn!("bandwidth window state persist failed: {}", e);
            }
        }
        Err(e) => warn!("bandwidth window state encode failed: {}", e),
    }
}

fn encode_bandwidth_state(
    current_window: &str,
    window_stats: &HashMap<i64, BandwidthWindowStat>,
    pending_windows: &[PendingWindow],
    segment_started_at: i64,
) -> serde_json::Result<Vec<u8>> {
    let state = StoredBandwidthState {
        current_window: current_window.to_string(),
        window_stats: window_stats.values().cloned().collect(),
        pending_windows: pending_windows
            .iter()
            .map(StoredPendingWindow::from)
            .collect(),
        boot: crate::telemetry::process_boot().to_string(),
        segment_started_at,
        pending_stats: Vec::new(),
    };
    serde_json::to_vec(&state)
}

/// `day`(YYYYMMDD) + `time_at`(HHMM) as produced by the reporter — the same
/// local-time rendering the control plane parses back for window identity.
fn parse_window_start(day: &str, time_at: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(&format!("{}{}", day, time_at), "%Y%m%d%H%M")
        .ok()
        .and_then(|dt| dt.and_local_timezone(chrono::Local).single())
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

fn bandwidth_stat_pb(
    stat: &BandwidthWindowStat,
    config_store: &ConfigStore,
    node_region_id: i64,
) -> pb::ServerBandwidthStat {
    let bytes_per_sec = bandwidth_bytes_per_sec(stat, config_store);
    pb::ServerBandwidthStat {
        user_id: stat.user_id,
        server_id: stat.server_id,
        day: stat.day.clone(),
        time_at: stat.time_at.clone(),
        bytes: bytes_per_sec as i64,
        bits: (bytes_per_sec * 8) as i64,
        total_bytes: stat.total_bytes as i64,
        bytes_in: stat.bytes_in as i64,
        cached_bytes: stat.cached_bytes.min(stat.total_bytes) as i64,
        attack_bytes: stat.attack_bytes.min(stat.total_bytes) as i64,
        count_requests: stat.count_requests as i64,
        count_cached_requests: stat.count_cached_requests as i64,
        count_attack_requests: stat.count_attack_requests as i64,
        user_plan_id: stat.user_plan_id,
        count_websocket_connections: stat.count_websocket_connections as i64,
        origin_total_bytes: stat.origin_total_bytes as i64,
        origin_avg_bytes: stat.origin_avg_bytes as i64,
        origin_avg_bits: stat.origin_avg_bits as i64,
        count_i_ps: crate::metrics::daily::UNIQUE_IP_TRACKER.count(stat.server_id, &stat.day),
        node_region_id,
        ..Default::default()
    }
}

fn drain_bandwidth_window(
    config_store: &ConfigStore,
    window_stats: &mut HashMap<i64, BandwidthWindowStat>,
    window_start: i64,
    segment_started_at: i64,
    node_region_id: i64,
    source_boot: String,
) -> PendingWindow {
    let mut error_counts = HashMap::new();
    let stats = window_stats
        .drain()
        .map(|(_, stat)| {
            if stat.count_errors > 0 {
                error_counts.insert(stat.server_id, stat.count_errors);
            }
            bandwidth_stat_pb(&stat, config_store, node_region_id)
        })
        .collect();
    PendingWindow {
        window_start,
        segment_started_at,
        source_boot,
        stats,
        error_counts,
    }
}

/// Offer a closed window to the bounded backlog. A full backlog drops the
/// window as an explicitly counted coverage gap; it never silently disappears.
/// Returns the gap to journal when the window was not retained.
fn retain_pending_window(
    pending: &mut Vec<PendingWindow>,
    window: PendingWindow,
) -> Option<PendingWindow> {
    if pending.len() < MAX_PENDING_BANDWIDTH_WINDOWS {
        pending.push(window);
        None
    } else {
        Some(window)
    }
}

fn drop_pending_window_gap(window: &PendingWindow) -> pb::TelemetryBatch {
    pb::TelemetryBatch {
        kind: "gap".into(),
        has_gap: true,
        gap_kind: "bandwidth".into(),
        gap_reason: "bandwidth durable backlog at its 12-window cap; window coverage lost".into(),
        window_start: window.window_start,
        occurred_at: crate::utils::time::now_timestamp(),
        ..Default::default()
    }
}

/// Split one closed window by server. The API validates every row in a batch,
/// so a single server with a stale assignment must not reject unrelated
/// servers' billable rows. The window key and event attribution stay unchanged;
/// only the wire grouping is narrowed.
fn window_batches(window: &PendingWindow, complete: bool) -> Vec<pb::TelemetryBatch> {
    let mut by_server: BTreeMap<i64, Vec<pb::ServerBandwidthStat>> = BTreeMap::new();
    for stat in &window.stats {
        by_server
            .entry(stat.server_id)
            .or_default()
            .push(stat.clone());
    }
    by_server
        .into_iter()
        .map(|(server_id, stats)| {
            let request = pb::UploadServerBandwidthStatsRequest {
                server_bandwidth_stats: stats,
            };
            let mut batch = pb::TelemetryBatch {
                kind: "bandwidth".into(),
                payload: prost::Message::encode_to_vec(&request),
                occurred_at: crate::utils::time::now_timestamp(),
                window_start: window.window_start,
                window_complete: complete,
                error_counts: window
                    .error_counts
                    .get(&server_id)
                    .copied()
                    .map(|count| [(server_id, count)].into_iter().collect())
                    .unwrap_or_default(),
                segment_started_at: window.segment_started_at,
                ..Default::default()
            };
            if !window.source_boot.is_empty() {
                batch.source_boot_id = window.source_boot.clone();
            }
            batch
        })
        .collect()
}

async fn enqueue_window_batches(window: &PendingWindow, complete: bool) -> anyhow::Result<()> {
    for batch in window_batches(window, complete) {
        crate::telemetry::enqueue_prepared(batch).await?;
    }
    Ok(())
}

/// Durable handoff for a split window: enqueue every server batch, then commit
/// the next backlog state with the last one. A crash before the state commit
/// replays already-enqueued server batches idempotently.
async fn enqueue_window_batches_with_state(
    window: &PendingWindow,
    complete: bool,
    state_key: &str,
    next_state: Vec<u8>,
) -> anyhow::Result<()> {
    let mut batches = window_batches(window, complete);
    let Some(last) = batches.pop() else {
        crate::telemetry::save_state(state_key, next_state).await?;
        return Ok(());
    };
    for batch in batches {
        crate::telemetry::enqueue_prepared(batch).await?;
    }
    crate::telemetry::enqueue_prepared_with_state(last, state_key, next_state).await
}

/// A window that closed while the process was down keeps partial coverage:
/// upload the accumulation as its own boot segment (never a completion
/// marker) and flag the downtime remainder as unrecoverable.
async fn flush_stale_window(
    config_store: &ConfigStore,
    current_window: &mut String,
    window_stats: &mut HashMap<i64, BandwidthWindowStat>,
    pending_windows: &mut Vec<PendingWindow>,
    stored_boot: &str,
    segment_started_at: i64,
) {
    if current_window.is_empty() {
        return;
    }
    let now = crate::utils::time::now_local();
    let day = now.format("%Y%m%d").to_string();
    let minute_floor = (now.minute() / 5) * 5;
    let live_key = format!("{}@{:02}{:02}", day, now.hour(), minute_floor);
    if *current_window == live_key {
        return;
    }
    let node_region_id = config_store.get_node_region_id().await;
    let window_start = window_stats
        .values()
        .next()
        .map(|stat| parse_window_start(&stat.day, &stat.time_at))
        .unwrap_or(0);
    let window = drain_bandwidth_window(
        config_store,
        window_stats,
        window_start,
        segment_started_at,
        node_region_id,
        stored_boot.to_string(),
    );
    if window.stats.is_empty() {
        *current_window = String::new();
        return;
    }
    if let Err(e) = enqueue_window_batches(&window, false).await {
        warn!(
            "stale bandwidth window could not reach the durable queue: {}",
            e
        );
        // The closed window still owns billable data. Keep it in the bounded
        // backlog for retry instead of discarding it because the spool refused
        // the first attempt.
        if let Some(dropped) = retain_pending_window(pending_windows, window) {
            crate::telemetry::record_gap(
                "bandwidth",
                dropped.window_start,
                "stale bandwidth window exceeded the restore backlog cap",
            );
        }
    }
    // The downtime span is permanent loss: emit a gap batch whose range covers
    // the stale window so settlement for it stays paused. It does not replace
    // the stale window's retained rows; those still carry their own coverage.
    let gap = pb::TelemetryBatch {
        kind: "gap".into(),
        has_gap: true,
        gap_kind: "bandwidth".into(),
        gap_reason: "node restarted before window close; downtime coverage unrecoverable".into(),
        window_start,
        occurred_at: crate::utils::time::now_timestamp(),
        ..Default::default()
    };
    if let Err(e) = crate::telemetry::enqueue_prepared(gap).await {
        warn!("restart gap marker could not be persisted: {}", e);
    }
    *current_window = String::new();
}

struct ReportingBaseline {
    metrics: Weak<ServerMetrics>,
    snapshot: ServerStatusSnapshot,
}

fn reporting_prior(
    last: &HashMap<u64, ReportingBaseline>,
    metrics: &Arc<ServerMetrics>,
    instance_id: u64,
) -> Option<ServerStatusSnapshot> {
    last.get(&instance_id).and_then(|baseline| {
        baseline
            .metrics
            .upgrade()
            .filter(|previous| Arc::ptr_eq(previous, metrics))
            .map(|_| baseline.snapshot.clone())
    })
}

fn snapshot_delta(
    current: &ServerStatusSnapshot,
    last: Option<&ServerStatusSnapshot>,
) -> ServerStatusSnapshot {
    let delta = |field: fn(&ServerStatusSnapshot) -> u64| -> u64 {
        let current_value = field(current);
        let last_value = last.map(field).unwrap_or(0);
        current_value.saturating_sub(last_value)
    };

    ServerStatusSnapshot {
        instance_id: current.instance_id,
        server_id: current.server_id,
        count_errors: delta(|s| s.count_errors),
        user_id: current.user_id,
        user_plan_id: current.user_plan_id,
        plan_id: current.plan_id,
        total_requests: delta(|s| s.total_requests),
        active_connections: current.active_connections,
        bytes_sent: delta(|s| s.bytes_sent),
        bytes_received: delta(|s| s.bytes_received),
        cached_bytes: delta(|s| s.cached_bytes),
        attack_bytes: delta(|s| s.attack_bytes),
        count_cached_requests: delta(|s| s.count_cached_requests),
        count_attack_requests: delta(|s| s.count_attack_requests),
        count_websocket_connections: delta(|s| s.count_websocket_connections),
        origin_bytes_sent: delta(|s| s.origin_bytes_sent),
        origin_bytes_received: delta(|s| s.origin_bytes_received),
        count_ips: current.count_ips,
    }
}

pub async fn start_bandwidth_reporter(config_store: ConfigStore) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        BANDWIDTH_SAMPLE_INTERVAL_SECS,
    ));
    let mut last_snapshots: HashMap<u64, ReportingBaseline> = HashMap::new();
    let (
        mut current_window,
        mut window_stats,
        mut pending_windows,
        stored_boot,
        mut segment_started_at,
    ) = load_bandwidth_state();
    flush_stale_window(
        &config_store,
        &mut current_window,
        &mut window_stats,
        &mut pending_windows,
        &stored_boot,
        segment_started_at,
    )
    .await;

    loop {
        interval.tick().await;
        let snapshots = crate::metrics::METRICS.take_reporting_snapshots();

        let now = crate::utils::time::now_local();
        let day = now.format("%Y%m%d").to_string();
        let minute_floor = (now.minute() / 5) * 5;
        let time_at = format!("{:02}{:02}", now.hour(), minute_floor);
        let window_started_at = now
            .with_second(0)
            .and_then(|dt| dt.with_minute(minute_floor))
            .map(|dt| dt.timestamp())
            .unwrap_or_else(|| now.timestamp());
        let window_key = format!("{}@{}", day, time_at);

        if current_window.is_empty() {
            current_window = window_key.clone();
            segment_started_at = window_started_at;
        } else if current_window != window_key {
            let node_region_id = config_store.get_node_region_id().await;
            let closed_start = window_started_at - BANDWIDTH_REPORT_INTERVAL_SECS as i64;
            // The closed window is pushed as a pending entry before draining:
            // a spool that cannot accept it keeps complete data for the next
            // rollover instead of dropping it.
            let closed = drain_bandwidth_window(
                &config_store,
                &mut window_stats,
                closed_start,
                segment_started_at,
                node_region_id,
                crate::telemetry::process_boot().to_string(),
            );
            if let Some(dropped) = retain_pending_window(&mut pending_windows, closed) {
                warn!(
                    window_start = dropped.window_start,
                    rows = dropped.stats.len(),
                    cap = MAX_PENDING_BANDWIDTH_WINDOWS,
                    "bandwidth backlog full; recording a coverage gap"
                );
                crate::telemetry::record_gap(
                    "bandwidth",
                    dropped.window_start,
                    "bandwidth durable backlog at its 12-window cap",
                );
                if let Err(error) =
                    crate::telemetry::enqueue_prepared(drop_pending_window_gap(&dropped)).await
                {
                    warn!(%error, "bandwidth backlog overflow gap could not be persisted");
                }
            }
            while let Some(window) = pending_windows.first() {
                let next_state = match encode_bandwidth_state(
                    &window_key,
                    &window_stats,
                    &pending_windows[1..],
                    window_started_at,
                ) {
                    Ok(raw) => raw,
                    Err(error) => {
                        warn!(%error, "bandwidth handoff state encode failed");
                        break;
                    }
                };
                if let Err(error) = enqueue_window_batches_with_state(
                    window,
                    true,
                    BANDWIDTH_STATE_STORAGE_KEY,
                    next_state,
                )
                .await
                {
                    warn!(%error, "bandwidth durable handoff failed; retaining window");
                    break;
                }
                pending_windows.remove(0);
            }
            current_window = window_key.clone();
            segment_started_at = window_started_at;
            persist_bandwidth_state(
                &current_window,
                &window_stats,
                &pending_windows,
                segment_started_at,
            )
            .await;
        }

        for sample in snapshots {
            let crate::metrics::ReportingSnapshot {
                metrics,
                snapshot: snap,
                retired,
            } = sample;
            let instance_id = snap.instance_id;
            let fingerprint = snap.fingerprint();
            let prior = reporting_prior(&last_snapshots, &metrics, instance_id);
            last_snapshots.insert(
                instance_id,
                ReportingBaseline {
                    metrics: Arc::downgrade(&metrics),
                    snapshot: snap.clone(),
                },
            );
            drop(metrics);
            // A live server's first sample has no baseline; skipping it avoids
            // counting the process-cumulative total as one sample. A retired
            // instance gets no later sample, so its full remaining counters are
            // the final delta instead of being discarded.
            if prior.is_none() && !retired {
                continue;
            }
            let delta = snapshot_delta(&snap, prior.as_ref());
            if retired {
                crate::metrics::METRICS.acknowledge_retired(
                    MetricConsumer::Bandwidth,
                    instance_id,
                    fingerprint,
                );
            }
            if delta.server_id <= 0 {
                continue;
            }

            let origin_total_bytes = delta.origin_bytes_received + delta.origin_bytes_sent;
            let billable_bytes = billable_bytes(&delta);
            let peak_bytes_per_sec = billable_bytes / BANDWIDTH_SAMPLE_INTERVAL_SECS;
            let stat = window_stats
                .entry(delta.server_id)
                .or_insert_with(|| BandwidthWindowStat {
                    day: day.clone(),
                    time_at: time_at.clone(),
                    user_id: delta.user_id,
                    server_id: delta.server_id,
                    user_plan_id: delta.user_plan_id,
                    ..Default::default()
                });
            stat.user_id = delta.user_id;
            stat.user_plan_id = delta.user_plan_id;
            stat.count_errors += delta.count_errors;
            stat.peak_bytes_per_sec = stat.peak_bytes_per_sec.max(peak_bytes_per_sec);
            stat.total_bytes += billable_bytes;
            stat.bytes_in += delta.bytes_received;
            stat.cached_bytes += delta.cached_bytes;
            stat.attack_bytes += delta.attack_bytes;
            stat.count_requests += delta.total_requests;
            stat.count_cached_requests += delta.count_cached_requests;
            stat.count_attack_requests += delta.count_attack_requests;
            stat.count_websocket_connections += delta.count_websocket_connections;
            stat.origin_total_bytes += origin_total_bytes;
            stat.origin_avg_bytes = stat.origin_total_bytes / BANDWIDTH_REPORT_INTERVAL_SECS;
            stat.origin_avg_bits = (stat.origin_total_bytes * 8) / BANDWIDTH_REPORT_INTERVAL_SECS;
            stat.count_ips = stat.count_ips.max(delta.count_ips);
        }
        last_snapshots.retain(|_, baseline| baseline.metrics.strong_count() > 0);
        persist_bandwidth_state(
            &current_window,
            &window_stats,
            &pending_windows,
            segment_started_at,
        )
        .await;
    }
}

const DAILY_PENDING_MAX_STATS: usize = 65_536;
const DAILY_PENDING_MAX_DOMAINS: usize = 65_536;
const DAILY_PENDING_MAX_BYTES: u64 = 64 * 1024 * 1024;

fn daily_pending_row_bytes<T: prost::Message>(row: &T) -> u64 {
    prost::Message::encoded_len(row) as u64 + std::mem::size_of::<T>() as u64
}

/// Bound the rows the daily reporter keeps after a failed persist. Server rows
/// are billing-relevant and get the larger share; domain detail is discardable
/// analytics. Oldest rows are kept, and the caller reports one telemetry gap
/// for the whole truncation.
fn cap_daily_pending(
    stats: &mut Vec<pb::ServerDailyStat>,
    domains: &mut Vec<pb::upload_server_daily_stats_request::DomainStat>,
) -> u64 {
    cap_daily_pending_with_limits(
        stats,
        domains,
        DAILY_PENDING_MAX_STATS,
        DAILY_PENDING_MAX_DOMAINS,
        DAILY_PENDING_MAX_BYTES,
    )
}

fn cap_daily_pending_with_limits(
    stats: &mut Vec<pb::ServerDailyStat>,
    domains: &mut Vec<pb::upload_server_daily_stats_request::DomainStat>,
    max_stats: usize,
    max_domains: usize,
    max_bytes: u64,
) -> u64 {
    let mut bytes = 0u64;
    let mut kept_stats = 0usize;
    for stat in stats.iter() {
        let row_bytes = daily_pending_row_bytes(stat);
        if kept_stats >= max_stats || bytes.saturating_add(row_bytes) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(row_bytes);
        kept_stats += 1;
    }
    let dropped_stats = stats.len().saturating_sub(kept_stats);
    stats.truncate(kept_stats);

    let mut kept_domains = 0usize;
    for domain in domains.iter() {
        let row_bytes = daily_pending_row_bytes(domain);
        if kept_domains >= max_domains || bytes.saturating_add(row_bytes) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(row_bytes);
        kept_domains += 1;
    }
    let dropped_domains = domains.len().saturating_sub(kept_domains);
    domains.truncate(kept_domains);
    (dropped_stats + dropped_domains) as u64
}

pub async fn start_daily_stat_reporter(config_store: ConfigStore) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        BANDWIDTH_SAMPLE_INTERVAL_SECS,
    ));
    let mut last_snapshots: HashMap<u64, ReportingBaseline> = HashMap::new();
    let mut current_window = String::new();
    let mut window_stats: HashMap<i64, pb::ServerDailyStat> = HashMap::new();
    let mut pending_stats: Vec<pb::ServerDailyStat> = Vec::new();
    let mut pending_domain_stats: Vec<pb::upload_server_daily_stats_request::DomainStat> =
        Vec::new();

    loop {
        interval.tick().await;
        let snapshots = crate::metrics::METRICS.take_reporting_snapshots();

        let now = crate::utils::time::now_local();
        let day = now.format("%Y%m%d").to_string();
        let minute_floor = (now.minute() / 5) * 5;
        let time_from = format!("{:02}{:02}", now.hour(), minute_floor);
        let created_at = now
            .with_second(0)
            .and_then(|dt| dt.with_minute(minute_floor))
            .map(|dt| dt.timestamp())
            .unwrap_or_else(|| now.timestamp());
        let node_region_id = config_store.get_node_region_id().await;
        let window_key = format!("{}@{}", day, time_from);

        if current_window.is_empty() {
            current_window = window_key.clone();
        } else if current_window != window_key {
            let min_day = (now - ChronoDuration::days(2)).format("%Y%m%d").to_string();
            crate::metrics::daily::UNIQUE_IP_TRACKER.cleanup_before(&min_day);

            let mut stats: Vec<_> = std::mem::take(&mut pending_stats);
            stats.extend(window_stats.drain().map(|(_, mut stat)| {
                stat.cached_bytes = stat.cached_bytes.min(stat.bytes);
                stat.attack_bytes = stat.attack_bytes.min(stat.bytes);
                stat
            }));

            let mut domain_stats = std::mem::take(&mut pending_domain_stats);
            let mut domain_rows = crate::metrics::daily::DAILY_DOMAIN_TRACKER
                .flush_older_than(created_at)
                .into_iter()
                .map(|(server_id, created_at, domain, mut value)| {
                    value.cached_bytes = value.cached_bytes.min(value.bytes);
                    value.attack_bytes = value.attack_bytes.min(value.bytes);
                    pb::upload_server_daily_stats_request::DomainStat {
                        server_id,
                        domain,
                        bytes: value.bytes,
                        cached_bytes: value.cached_bytes,
                        count_requests: value.count_requests,
                        count_cached_requests: value.count_cached_requests,
                        count_attack_requests: value.count_attack_requests,
                        attack_bytes: value.attack_bytes,
                        created_at,
                    }
                })
                .collect::<Vec<_>>();
            domain_rows.sort_by(|a, b| {
                a.server_id
                    .cmp(&b.server_id)
                    .then_with(|| b.count_requests.cmp(&a.count_requests))
            });
            let mut per_server_counts: HashMap<i64, usize> = HashMap::new();
            for item in domain_rows {
                let count = per_server_counts.entry(item.server_id).or_default();
                if *count < 20 {
                    domain_stats.push(item);
                    *count += 1;
                }
            }

            if !stats.is_empty() || !domain_stats.is_empty() {
                let request = pb::UploadServerDailyStatsRequest {
                    stats,
                    domain_stats,
                };
                if let Err(error) = crate::telemetry::enqueue_daily_stats(request).await {
                    let source = error.to_string();
                    let lost = error.lost();
                    let unsent = error.into_unsent();
                    pending_stats = unsent.stats;
                    pending_domain_stats = unsent.domain_stats;
                    let mut dropped = lost as u64;
                    dropped += cap_daily_pending(&mut pending_stats, &mut pending_domain_stats);
                    warn!(
                        lost,
                        pending_stats = pending_stats.len(),
                        pending_domains = pending_domain_stats.len(),
                        error = %source,
                        "failed to persist daily stats; unpersisted rows retained"
                    );
                    if dropped > 0 {
                        crate::telemetry::record_gap(
                            "daily",
                            now.timestamp(),
                            "daily_pending_capacity",
                        );
                    }
                }
            }
            current_window = window_key.clone();
        }

        for sample in snapshots {
            let crate::metrics::ReportingSnapshot {
                metrics,
                snapshot: snap,
                retired,
            } = sample;
            let instance_id = snap.instance_id;
            let fingerprint = snap.fingerprint();
            let prior = reporting_prior(&last_snapshots, &metrics, instance_id);
            last_snapshots.insert(
                instance_id,
                ReportingBaseline {
                    metrics: Arc::downgrade(&metrics),
                    snapshot: snap.clone(),
                },
            );
            drop(metrics);
            // Skip the first live sample for the same reason as the bandwidth
            // reporter; a retired instance has no later sample, so its full
            // remaining counters are consumed before acknowledgement.
            if prior.is_none() && !retired {
                continue;
            }
            let delta = snapshot_delta(&snap, prior.as_ref());
            if delta.server_id <= 0 {
                continue;
            }

            let check_traffic_limiting = config_store
                .get_server_by_id(delta.server_id)
                .await
                .map(|server| server.has_valid_traffic_limit())
                .unwrap_or(false);

            let stat = window_stats
                .entry(delta.server_id)
                .or_insert_with(|| pb::ServerDailyStat {
                    server_id: delta.server_id,
                    user_id: delta.user_id,
                    node_region_id,
                    created_at,
                    check_traffic_limiting,
                    plan_id: delta.plan_id,
                    ..Default::default()
                });
            stat.user_id = delta.user_id;
            stat.node_region_id = node_region_id;
            stat.created_at = created_at;
            stat.check_traffic_limiting = check_traffic_limiting;
            stat.plan_id = delta.plan_id;
            stat.bytes += billable_bytes(&delta) as i64;
            stat.cached_bytes += delta.cached_bytes as i64;
            stat.count_requests += delta.total_requests as i64;
            stat.count_cached_requests += delta.count_cached_requests as i64;
            stat.count_attack_requests += delta.count_attack_requests as i64;
            stat.attack_bytes += delta.attack_bytes as i64;
            if retired {
                crate::metrics::METRICS.acknowledge_retired(
                    MetricConsumer::Daily,
                    instance_id,
                    fingerprint,
                );
            }
        }
        last_snapshots.retain(|_, baseline| baseline.metrics.strong_count() > 0);
    }
}

// Cumulative snapshot rows carry only the frozen item's selected value; the
// old incremental sample type is gone.

/// One request plus the exact snapshot identities it carries, so a successful
/// durable append can advance only those rows' uploaded revisions.
struct SnapshotChunk {
    request: pb::UploadMetricStatsRequest,
    rows: Vec<(SnapshotKey, u64)>,
}

/// Enqueue seam. The reporter and its tests both drive the same
/// build/split/retry logic; only the durable sink differs.
trait MetricUploadSink {
    async fn send(&mut self, request: &pb::UploadMetricStatsRequest) -> anyhow::Result<()>;
}

struct TelemetryMetricUploadSink;

impl MetricUploadSink for TelemetryMetricUploadSink {
    async fn send(&mut self, request: &pb::UploadMetricStatsRequest) -> anyhow::Result<()> {
        crate::telemetry::enqueue("metric_stat", request, 0, false).await
    }
}

/// Wire budget for the serialized `UploadMetricStatsRequest` payload.
///
/// Telemetry caps the finished `TelemetryBatch` at its private
/// `MAX_BATCH_ENCODED_BYTES` (3/4 of `RPC_NODE_STREAM_MESSAGE_BYTES`). Until
/// telemetry exposes its per-kind budget, this mirrors both the cap and the
/// worst-case envelope measurement (`encoded_len`, no encoding) so a chunk that
/// fits here also fits the spool's check.
fn metric_stat_payload_budget() -> usize {
    const MAX_BATCH_ENCODED_BYTES: usize =
        crate::rpc::client::RPC_NODE_STREAM_MESSAGE_BYTES / 4 * 3;
    const PAYLOAD_FIELD_OVERHEAD: usize = 1 + 5;

    let binding = crate::telemetry::binding();
    let envelope = pb::TelemetryBatch {
        schema_version: 1,
        batch_id: format!("{}:{:020}", "0".repeat(36), u64::MAX),
        boot_id: "0".repeat(36),
        sequence: u64::MAX,
        kind: "metric_stat".into(),
        occurred_at: i64::MIN,
        window_start: i64::MIN,
        source_boot_id: "0".repeat(36),
        assignment_version: binding.assignment_version,
        route_version: binding.route_version,
        ..Default::default()
    };
    MAX_BATCH_ENCODED_BYTES
        .saturating_sub(prost::Message::encoded_len(&envelope))
        .saturating_sub(PAYLOAD_FIELD_OVERHEAD)
}

fn snapshot_window_end(period_end: i64, window_start: i64, last_event_millis: i64) -> i64 {
    if last_event_millis <= 0 {
        return period_end.max(window_start);
    }
    let actual = (last_event_millis / 1000).saturating_add(1);
    period_end.min(actual).max(window_start.saturating_add(1))
}

/// Turn one drained plan row into a cumulative delta. The plan's frozen item
/// definition chooses the uploaded value; a retired plan cannot be reinterpreted
/// by a newer configuration.
fn collect_plan_deltas(
    plans: &[std::sync::Arc<crate::metrics::plan::MetricCollectionPlan>],
) -> (Vec<crate::rpc::metric_stat_cache::SnapshotDelta>, usize) {
    let mut deltas = Vec::new();
    let mut skipped = 0usize;
    for plan in plans {
        let Some(aggregator) = plan.aggregator() else {
            continue;
        };
        let drain = aggregator.drain();
        for (key, entry) in drain.rows {
            let Some(item) = plan.item(key.item_id, key.version) else {
                crate::metrics::projection::METRIC_PROJECTION_POOL.refund(entry.charge);
                skipped += 1;
                continue;
            };
            deltas.push(crate::rpc::metric_stat_cache::SnapshotDelta {
                key: SnapshotKey {
                    item_id: key.item_id,
                    version: key.version,
                    time_key: key.time_key.to_string(),
                    window_start: key.window_start,
                    period_end: key.window_end,
                    server_id: key.server_id,
                    keys: key.keys.to_vec(),
                },
                window_end: snapshot_window_end(
                    key.window_end,
                    key.window_start,
                    entry.last_event_time_millis,
                ),
                value: item.value.select(&entry.counters),
                charge: entry.charge,
            });
        }
    }
    (deltas, skipped)
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct SnapshotHeader {
    server_id: i64,
    item_id: i64,
    version: i32,
    time_key: String,
    window_start: i64,
    /// One request-level revision must not cover rows with different row
    /// revisions: a delayed group could then arrive with the same revision as a
    /// newer value and ReplacingMergeTree would choose arbitrarily. Rows with
    /// different revisions are separate wire groups.
    revision: u64,
}

/// Group dirty rows by wire header. Rows keep their own revision for checkpoint
/// bookkeeping; the request carries the maximum revision in the group, which is
/// safe because every payload is a full cumulative value at that identity.
fn build_snapshot_chunks(
    rows: Vec<DirtySnapshotRow>,
    epoch: &str,
    budget: usize,
) -> (Vec<SnapshotChunk>, Vec<SnapshotChunk>) {
    let mut grouped: BTreeMap<SnapshotHeader, Vec<DirtySnapshotRow>> = BTreeMap::new();
    for row in rows {
        grouped
            .entry(SnapshotHeader {
                server_id: row.key.server_id,
                item_id: row.key.item_id,
                version: row.key.version,
                time_key: row.key.time_key.clone(),
                window_start: row.key.window_start,
                revision: row.revision,
            })
            .or_default()
            .push(row);
    }

    let mut chunks = Vec::new();
    let mut quarantine = Vec::new();
    for (header, group) in grouped {
        let window_end = group.iter().map(|row| row.window_end).max().unwrap_or(0);
        let request_revision = header.revision;
        let count = group.len() as i64;
        let total = group.iter().map(|row| row.value).sum::<f32>();
        let mut records = Vec::with_capacity(group.len());
        for row in &group {
            let hash = row.key.hash(epoch, row.incarnation);
            records.push((
                pb::UploadingMetricStat {
                    id: 0,
                    hash,
                    keys: row.key.keys.clone(),
                    value: row.value,
                },
                (row.key.clone(), row.revision),
            ));
        }

        let make_request = |stats: Vec<pb::UploadingMetricStat>| pb::UploadMetricStatsRequest {
            server_id: header.server_id,
            time: header.time_key.clone(),
            count,
            total,
            version: header.version,
            item_id: header.item_id,
            metric_stats: stats,
            keep_keys: Vec::new(),
            revision: request_revision,
            window_start: header.window_start,
            window_end,
        };
        let header_cost = prost::Message::encoded_len(&make_request(Vec::new()));
        let mut current = Vec::new();
        let mut current_rows = Vec::new();
        let mut used = header_cost;
        for (stat, identity) in records {
            let cost = prost::encoding::message::encoded_len(7, &stat);
            if header_cost.saturating_add(cost) > budget {
                quarantine.push(SnapshotChunk {
                    request: make_request(vec![stat]),
                    rows: vec![identity],
                });
                continue;
            }
            if used.saturating_add(cost) > budget && !current.is_empty() {
                chunks.push(SnapshotChunk {
                    request: make_request(std::mem::take(&mut current)),
                    rows: std::mem::take(&mut current_rows),
                });
                used = header_cost;
            }
            used = used.saturating_add(cost);
            current.push(stat);
            current_rows.push(identity);
        }
        if !current.is_empty() {
            chunks.push(SnapshotChunk {
                request: make_request(current),
                rows: current_rows,
            });
        }
    }
    (chunks, quarantine)
}

#[derive(Default)]
struct MetricStatTickOutcome {
    merged: usize,
    dropped: usize,
    pruned: usize,
    enqueued: usize,
    quarantined: usize,
    deferred: usize,
}

async fn quarantine_snapshot_chunk(
    state: &mut MetricStatReporterState,
    chunk: SnapshotChunk,
) -> bool {
    match state.quarantine(&chunk.request).await {
        Ok(bytes) => {
            crate::telemetry::record_gap(
                "metric_stat",
                crate::utils::time::now_timestamp(),
                "metric_stat_record_over_batch_cap",
            );
            warn!(
                server_id = chunk.request.server_id,
                item_id = chunk.request.item_id,
                bytes,
                dir = %state.quarantine_dir().display(),
                "metric stat snapshot record durably quarantined locally"
            );
            state.mark_uploaded(&chunk.rows);
            true
        }
        Err(e) => {
            state.record_retry_refusal();
            warn!(
                server_id = chunk.request.server_id,
                item_id = chunk.request.item_id,
                "local metric stat quarantine refused the snapshot record; row retained dirty: {e}"
            );
            false
        }
    }
}

/// Reporter tick: drain every plan generation into the cumulative snapshot
/// store, fsync the checkpoint, then upload dirty rows. A checkpoint failure or
/// a spool failure leaves rows dirty and observable; no accepted projection is
/// dropped.
async fn run_metric_stat_tick<S: MetricUploadSink>(
    state: &mut MetricStatReporterState,
    _metric_items: &[crate::config_models::MetricItemConfig],
    plans: &[std::sync::Arc<crate::metrics::plan::MetricCollectionPlan>],
    now: &DateTime<FixedOffset>,
    sink: &mut S,
) -> MetricStatTickOutcome {
    let (deltas, skipped) = collect_plan_deltas(plans);
    let merge = state.merge(deltas);
    let mut outcome = MetricStatTickOutcome {
        merged: merge.created.saturating_add(merge.updated),
        dropped: merge.dropped.saturating_add(skipped),
        ..Default::default()
    };
    let _ = merge.released_bytes;

    // Checkpoint before upload: a crash after a successful append but before
    // the uploaded marker is a same-revision replay, never new data. A crash
    // before this checkpoint can lose projections accepted since the previous
    // checkpoint (they are in memory only); it never sends a partial snapshot.
    state.set_backpressured(false);
    if let Err(e) = state.persist_checkpoint().await {
        warn!(
            checkpoint = %state.checkpoint_path().display(),
            "metric snapshot checkpoint refused; no upload attempted and dirty rows retained: {e}"
        );
        state.set_backpressured(true);
        outcome.deferred = state.dirty_len();
        state.publish_stats();
        return outcome;
    }

    const SNAPSHOT_UPLOAD_PAGE: usize = 1024;
    let budget = metric_stat_payload_budget();
    let mut upload_failed = false;
    loop {
        let page = state.dirty_rows_page(
            SNAPSHOT_UPLOAD_PAGE,
            crate::rpc::metric_stat_cache::DEFAULT_DIRTY_PAGE_BYTES,
        );
        if page.is_empty() {
            break;
        }
        let dirty_before = state.dirty_len();
        let (chunks, quarantine) = build_snapshot_chunks(page, state.epoch(), budget);
        for chunk in quarantine {
            if quarantine_snapshot_chunk(state, chunk).await {
                outcome.quarantined += 1;
            } else {
                outcome.deferred += 1;
                upload_failed = true;
            }
        }
        if upload_failed {
            break;
        }
        for chunk in chunks {
            match sink.send(&chunk.request).await {
                Ok(()) => {
                    state.mark_uploaded(&chunk.rows);
                    outcome.enqueued += 1;
                }
                Err(e) if crate::telemetry::is_batch_too_large(e.as_ref()) => {
                    warn!(
                        server_id = chunk.request.server_id,
                        item_id = chunk.request.item_id,
                        "metric snapshot chunk exceeded the wire at the spool; retaining in durable quarantine: {e}"
                    );
                    if quarantine_snapshot_chunk(state, chunk).await {
                        outcome.quarantined += 1;
                    } else {
                        outcome.deferred += 1;
                        upload_failed = true;
                        break;
                    }
                }
                Err(e) => {
                    if !crate::telemetry::is_capacity_exhausted(e.as_ref()) {
                        state.record_retry_refusal();
                    }
                    warn!(
                        server_id = chunk.request.server_id,
                        item_id = chunk.request.item_id,
                        "metric snapshot upload failed; dirty rows retained: {e}"
                    );
                    outcome.deferred += 1;
                    upload_failed = true;
                    break;
                }
            }
        }
        if upload_failed || state.dirty_len() >= dirty_before {
            // No progress: leave the remaining pages for the next tick rather
            // than spinning on a spool that is refusing every chunk.
            break;
        }
    }

    // Marking uploaded is not durable until this checkpoint lands. A failure
    // here re-uploads the same full rows at the same revisions.
    if let Err(e) = state.persist_checkpoint().await {
        warn!(
            checkpoint = %state.checkpoint_path().display(),
            "metric snapshot post-upload checkpoint failed; same-revision replay expected: {e}"
        );
        state.set_backpressured(true);
    }
    outcome.pruned = state.prune(now.timestamp());
    state.publish_stats();
    outcome
}

/// Current size of the metric snapshot state. Exposed for memory governance and
/// node observability; values are the reporter's own counters.
pub fn metric_values_cache_stats() -> MetricValuesCacheStats {
    crate::rpc::metric_stat_cache::metric_values_cache_stats()
}

pub async fn start_metric_stat_reporter(config_store: Arc<crate::config::ConfigStore>) {
    let dir = crate::paths::NodePaths::current()
        .data_dir()
        .join("metric_snapshot");
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let mut sink = TelemetryMetricUploadSink;
    let mut state: Option<MetricStatReporterState> = None;

    loop {
        interval.tick().await;
        if state.is_none() {
            match MetricStatReporterState::open(&dir) {
                Ok(opened) => {
                    info!(
                        dir = %dir.display(),
                        "metric snapshot store opened; projection admission resumed"
                    );
                    state = Some(opened);
                }
                Err(e) => {
                    // Keep the checkpoint and retry without a process restart.
                    // Plan admission of new identities is paused so the shared
                    // pool cannot fill while nothing drains it.
                    crate::rpc::metric_stat_cache::set_metric_snapshot_store_unavailable(true);
                    warn!(
                        dir = %dir.display(),
                        "metric snapshot store unavailable; checkpoint retained, new projection admission paused, retrying next tick: {e}"
                    );
                    continue;
                }
            }
        }
        let state = state.as_mut().expect("opened above");
        let now = crate::utils::time::now_local();
        let metric_items = config_store.get_metric_items().await;
        let plans = crate::metrics::plan::plans_for_drain();
        let outcome = run_metric_stat_tick(state, &metric_items, &plans, &now, &mut sink).await;
        crate::metrics::plan::retain_drained();

        debug!(
            merged = outcome.merged,
            enqueued = outcome.enqueued,
            pending = state.dirty_len(),
            "metric snapshot tick completed"
        );

        if outcome.dropped > 0 {
            warn!(
                dropped = outcome.dropped,
                entries = state.len(),
                "metric snapshot dropped new keys at the cardinality/byte cap"
            );
        }
        if outcome.deferred > 0 {
            warn!(
                deferred = outcome.deferred,
                pending = state.dirty_len(),
                checkpoint = %state.checkpoint_path().display(),
                "metric snapshot rows remain dirty; checkpoint retained"
            );
        }
        if outcome.quarantined > 0 {
            warn!(
                quarantined = outcome.quarantined,
                dir = %state.quarantine_dir().display(),
                "metric snapshot records retained in local quarantine"
            );
        }
        if outcome.pruned > 0 {
            debug!(
                pruned = outcome.pruned,
                "metric snapshot pruned closed windows"
            );
        }
    }
}

use std::sync::atomic::{AtomicBool, AtomicI32};

static LAST_NODE_LEVEL: AtomicI32 = AtomicI32::new(-1);
static LAST_HAS_PARENTS: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy)]
struct HttpRequestStatUploadLimits {
    max_cities: usize,
    max_providers: usize,
    max_systems: usize,
    max_browsers: usize,
}

impl HttpRequestStatUploadLimits {
    fn from_config(config: &crate::config_models::GlobalStatUploadConfig) -> Self {
        Self {
            max_cities: positive_or_default(config.max_cities, 32),
            max_providers: positive_or_default(config.max_providers, 32),
            max_systems: positive_or_default(config.max_systems, 64),
            max_browsers: positive_or_default(config.max_browsers, 64),
        }
    }
}

fn positive_or_default(value: i16, default_value: usize) -> usize {
    if value > 0 {
        value as usize
    } else {
        default_value
    }
}

fn trim_http_request_stat_rows_with_limits(
    req: &mut pb::UploadServerHttpRequestStatRequest,
    limits: HttpRequestStatUploadLimits,
) {
    req.region_cities
        .sort_by_key(|a| std::cmp::Reverse(a.count_requests));
    let mut server_counts = HashMap::<i64, usize>::new();
    req.region_cities.retain(|row| {
        let count = server_counts.entry(row.server_id).or_default();
        *count += 1;
        *count <= limits.max_cities
    });

    req.region_providers
        .sort_by_key(|a| std::cmp::Reverse(a.count));
    server_counts.clear();
    req.region_providers.retain(|row| {
        let count = server_counts.entry(row.server_id).or_default();
        *count += 1;
        *count <= limits.max_providers
    });

    req.systems.sort_by_key(|a| std::cmp::Reverse(a.count));
    server_counts.clear();
    req.systems.retain(|row| {
        let count = server_counts.entry(row.server_id).or_default();
        *count += 1;
        *count <= limits.max_systems
    });

    req.browsers.sort_by_key(|a| std::cmp::Reverse(a.count));
    server_counts.clear();
    req.browsers.retain(|row| {
        let count = server_counts.entry(row.server_id).or_default();
        *count += 1;
        *count <= limits.max_browsers
    });
}

/// Node level/parent topology is diagnostic logging only. It runs after the
/// local request-stat rows have been handed to the durable spool, so an
/// unreachable API can never delay or drop accepted request-stat counters.
async fn report_node_level_info(api_config: &ApiConfig) {
    let client = match SharedRpcClient::get(api_config).await {
        Ok(shared) => shared.as_rpc_client(),
        Err(e) => {
            warn!("Node level info unavailable: {}", e);
            return;
        }
    };
    let mut node_service = client.node_service();

    match node_service
        .find_node_level_info(pb::FindNodeLevelInfoRequest {})
        .await
    {
        Ok(resp) => {
            let info = resp.into_inner();
            let has_parents = !info.parent_nodes_map_json.is_empty();

            if info.level != LAST_NODE_LEVEL.load(Ordering::Relaxed)
                || has_parents != LAST_HAS_PARENTS.load(Ordering::Relaxed)
            {
                LAST_NODE_LEVEL.store(info.level, Ordering::Relaxed);
                LAST_HAS_PARENTS.store(has_parents, Ordering::Relaxed);
                info!(
                    "Node Level identified: {}, Parents: {}",
                    info.level, has_parents
                );
            } else {
                debug!(
                    "Node Level verified: {}, Parents: {}",
                    info.level, has_parents
                );
            }
        }
        Err(e) => {
            warn!("Failed to fetch node level info: {}", e);
        }
    }
}

type RequestStatRow = (
    crate::metrics::aggregator::AggregationKey,
    crate::metrics::aggregator::AggregatedValue,
);

fn group_request_stat_rows_by_server(
    rows: Vec<RequestStatRow>,
) -> BTreeMap<i64, Vec<RequestStatRow>> {
    let mut by_server: BTreeMap<i64, Vec<RequestStatRow>> = BTreeMap::new();
    for row in rows {
        by_server.entry(row.0.server_id).or_default().push(row);
    }
    by_server
}

fn build_http_request_stat_request(
    rows: &[RequestStatRow],
    month: &str,
    day: &str,
    limits: HttpRequestStatUploadLimits,
) -> pb::UploadServerHttpRequestStatRequest {
    let mut req = pb::UploadServerHttpRequestStatRequest {
        month: month.to_string(),
        day: day.to_string(),
        region_cities: vec![],
        region_providers: vec![],
        systems: vec![],
        browsers: vec![],
        http_firewall_rule_groups: vec![],
    };

    type CityStatKey = (i64, i64, i64, i64);
    type CityStatValue = (i64, i64, i64, i64);
    type CityStatMap = HashMap<CityStatKey, CityStatValue>;
    let mut city_map: CityStatMap = HashMap::new();
    let mut provider_map: HashMap<(i64, i64), i64> = HashMap::new();
    let mut system_map: HashMap<(i64, String, String), i64> = HashMap::new();
    let mut browser_map: HashMap<(i64, String, String), i64> = HashMap::new();
    let mut waf_map: HashMap<(i64, i64, String), i64> = HashMap::new();

    for (key, val) in rows {
        if key.country_id > 0 {
            let entry = city_map
                .entry((key.server_id, key.country_id, key.province_id, key.city_id))
                .or_default();
            entry.0 += val.count;
            entry.1 += val.bytes_sent;
            entry.2 += val.count_attack;
            entry.3 += val.attack_bytes;
        }

        if key.provider_id > 0 {
            *provider_map
                .entry((key.server_id, key.provider_id))
                .or_default() += val.count;
        }

        if !key.os.is_empty() {
            *system_map
                .entry((
                    key.server_id,
                    key.os.to_string(),
                    key.os_version.to_string(),
                ))
                .or_default() += val.count;
        }

        if !key.browser.is_empty() {
            *browser_map
                .entry((
                    key.server_id,
                    key.browser.to_string(),
                    key.browser_version.to_string(),
                ))
                .or_default() += val.count;
        }

        if key.waf_group_id > 0 && val.count_attack > 0 {
            *waf_map
                .entry((key.server_id, key.waf_group_id, key.waf_action.to_string()))
                .or_default() += val.count_attack;
        }
    }

    for ((server_id, country_id, province_id, city_id), value) in city_map {
        req.region_cities
            .push(pb::upload_server_http_request_stat_request::RegionCity {
                server_id,
                count_requests: value.0,
                bytes: value.1,
                count_attack_requests: value.2,
                attack_bytes: value.3,
                region_country_id: country_id,
                region_province_id: province_id,
                region_city_id: city_id,
            });
    }

    for ((server_id, provider_id), count) in provider_map {
        req.region_providers.push(
            pb::upload_server_http_request_stat_request::RegionProvider {
                server_id,
                count,
                region_provider_id: provider_id,
            },
        );
    }

    for ((server_id, name, version), count) in system_map {
        req.systems
            .push(pb::upload_server_http_request_stat_request::System {
                server_id,
                name,
                version,
                count,
            });
    }

    for ((server_id, name, version), count) in browser_map {
        req.browsers
            .push(pb::upload_server_http_request_stat_request::Browser {
                server_id,
                name,
                version,
                count,
            });
    }

    for ((server_id, waf_group_id, action), count) in waf_map {
        req.http_firewall_rule_groups.push(
            pb::upload_server_http_request_stat_request::HttpFirewallRuleGroup {
                server_id,
                http_firewall_rule_group_id: waf_group_id,
                action,
                count,
            },
        );
    }

    trim_http_request_stat_rows_with_limits(&mut req, limits);
    req
}

pub async fn start_metrics_aggregator_reporter(config_store: ConfigStore, api_config: ApiConfig) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        interval.tick().await;
        let drain = crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.drain();
        if drain.rows.is_empty() {
            crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR
                .release_charge(drain.charged_bytes);
            report_node_level_info(&api_config).await;
            continue;
        }

        // Known limitation (not changed this round): built-in request stats
        // are attributed to the flush wall-clock day/month, not each row's
        // event time. The aggregation key carries no event timestamp, so a
        // row that crosses midnight is reported under the flush day.
        let now = crate::utils::time::now_local();
        let month = now.format("%Y%m").to_string();
        let day = now.format("%Y%m%d").to_string();
        let upload_limits =
            HttpRequestStatUploadLimits::from_config(&config_store.get_global_stat_upload_sync());

        let by_server = group_request_stat_rows_by_server(drain.rows);

        // One durable enqueue per server. A server whose assignment the API
        // rejects must not take unrelated servers' billable rows with it.
        for (server_id, rows) in by_server {
            let charged_bytes = rows.iter().fold(0u64, |total, (_, value)| {
                total.saturating_add(value.estimated_bytes)
            });
            let req = build_http_request_stat_request(&rows, &month, &day, upload_limits);
            match crate::telemetry::enqueue("request_stat", &req, 0, false).await {
                Ok(()) => crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR
                    .release_charge(charged_bytes),
                Err(e) => {
                    warn!(
                        server_id,
                        error = %e,
                        "Failed to persist HTTP request stats for server; rows retained"
                    );
                    // Only this server's rows go back, and their charge was
                    // never refunded, so they are restored without recharging.
                    crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.restore_drained(
                        crate::metrics::aggregator::AggregatorDrain {
                            rows,
                            charged_bytes,
                        },
                    );
                }
            }
        }

        report_node_level_info(&api_config).await;
    }
}

fn group_top_ip_rows_by_server(
    rows: Vec<(i64, String, u64)>,
) -> BTreeMap<i64, Vec<(i64, String, u64)>> {
    let mut by_server = BTreeMap::new();
    for row in rows {
        by_server.entry(row.0).or_insert_with(Vec::new).push(row);
    }
    by_server
}

pub async fn start_top_ip_stat_reporter() {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));

    loop {
        interval.tick().await;
        let rows = crate::metrics::top_ip::TOP_IP_TRACKER.flush();
        if rows.is_empty() {
            continue;
        }

        let now = crate::utils::time::now_local();
        let day = now.format("%Y%m%d").to_string();
        let minute_floor = (now.minute() / 5) * 5;
        let time_at = format!("{:02}{:02}", now.hour(), minute_floor);

        for (server_id, rows) in group_top_ip_rows_by_server(rows) {
            let stats = rows
                .iter()
                .map(|(server_id, ip, count_requests)| {
                    pb::upload_server_top_ip_stats_request::Stat {
                        server_id: *server_id as u64,
                        ip: ip.clone(),
                        // u64 → u32 silent wraparound at 4.3B would mask runaway IPs;
                        // saturate to u32::MAX so the upper bound stays visible.
                        count_requests: u32::try_from(*count_requests).unwrap_or(u32::MAX),
                        day: day.clone(),
                        time_at: time_at.clone(),
                    }
                })
                .collect();

            if let Err(e) = crate::telemetry::enqueue(
                "top_ip",
                &pb::UploadServerTopIpStatsRequest { stats },
                0,
                false,
            )
            .await
            {
                warn!(server_id, error = %e, "Failed to persist top IP stats; server rows retained");
                crate::metrics::top_ip::TOP_IP_TRACKER.restore(&rows);
            }
        }
    }
}

#[cfg(test)]
mod reporting_baseline_tests {
    use super::*;

    #[test]
    fn baseline_identity_is_the_metric_instance_not_the_server_id() {
        let first = Arc::new(ServerMetrics::new());
        let second = Arc::new(ServerMetrics::new());
        let snapshot = first.snapshot();
        let mut baselines = HashMap::new();
        baselines.insert(
            first.instance_id,
            ReportingBaseline {
                metrics: Arc::downgrade(&first),
                snapshot,
            },
        );
        assert!(reporting_prior(&baselines, &first, first.instance_id).is_some());
        assert!(reporting_prior(&baselines, &second, second.instance_id).is_none());
        assert_ne!(first.instance_id, second.instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_ip_batches_preserve_rows_and_separate_servers() {
        let rows = vec![
            (1, "192.0.2.1".into(), 10),
            (2, "192.0.2.2".into(), 20),
            (1, "192.0.2.3".into(), 30),
        ];
        let groups = group_top_ip_rows_by_server(rows.clone());
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&1], vec![rows[0].clone(), rows[2].clone()]);
        assert_eq!(groups[&2], vec![rows[1].clone()]);
        assert_eq!(groups.values().flatten().map(|row| row.2).sum::<u64>(), 60);
    }

    fn snapshot_key(item_id: i64, server_id: i64, keys: &[&str]) -> SnapshotKey {
        SnapshotKey {
            item_id,
            version: 1,
            time_key: "202610311200".into(),
            window_start: 1_790_000_000,
            period_end: 1_790_000_060,
            server_id,
            keys: keys.iter().map(|key| (*key).to_string()).collect(),
        }
    }

    fn dirty(row: SnapshotKey, value: f32, revision: u64) -> DirtySnapshotRow {
        DirtySnapshotRow {
            key: row,
            window_end: 1_790_000_030,
            value,
            revision,
            incarnation: revision,
        }
    }

    fn request_stat_row(server_id: i64, city: &str, count: i64) -> RequestStatRow {
        use crate::metrics::aggregator::{AggregatedValue, AggregationKey};
        let empty = Arc::<str>::from("");
        (
            AggregationKey {
                category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
                server_id,
                country: Arc::from("United States"),
                country_id: 1,
                province: Arc::from("California"),
                province_id: 2,
                city: Arc::from(city),
                city_id: city.len() as i64,
                provider: empty.clone(),
                browser: empty.clone(),
                os: empty.clone(),
                waf_group_id: 0,
                waf_action: empty.clone(),
                provider_id: 0,
                browser_version: empty.clone(),
                os_version: empty,
            },
            AggregatedValue {
                count,
                ..Default::default()
            },
        )
    }

    #[test]
    fn request_stat_batches_are_split_per_server() {
        let rows = vec![
            request_stat_row(10, "Los Angeles", 3),
            request_stat_row(20, "San Diego", 5),
            request_stat_row(10, "San Jose", 7),
        ];
        let grouped = group_request_stat_rows_by_server(rows);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[&10].len(), 2);
        assert_eq!(grouped[&20].len(), 1);

        let limits = HttpRequestStatUploadLimits {
            max_cities: 32,
            max_providers: 32,
            max_systems: 64,
            max_browsers: 64,
        };
        let req = build_http_request_stat_request(&grouped[&10], "202610", "20261031", limits);
        assert_eq!(req.region_cities.len(), 2);
        assert!(req.region_cities.iter().all(|row| row.server_id == 10));
        assert_eq!(
            req.region_cities
                .iter()
                .map(|row| row.count_requests)
                .sum::<i64>(),
            10
        );
    }

    fn metric_item(
        id: i64,
        keys: Vec<&str>,
        value: &str,
    ) -> crate::config_models::MetricItemConfig {
        crate::config_models::MetricItemConfig {
            id,
            name: String::new(),
            code: String::new(),
            category: crate::metrics::METRIC_CATEGORY_HTTP.to_string(),
            keys: keys.into_iter().map(str::to_string).collect(),
            value: serde_json::Value::String(value.to_string()),
            period: 1,
            period_unit: "minute".to_string(),
            version: 1,
            is_on: true,
        }
    }

    #[test]
    fn daily_pending_cap_keeps_oldest_rows_within_count_and_byte_limits() {
        let mut stats = (0..5)
            .map(|server_id| pb::ServerDailyStat {
                server_id,
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let mut domains = (0..5)
            .map(
                |server_id| pb::upload_server_daily_stats_request::DomainStat {
                    server_id,
                    domain: format!("d{server_id}.example"),
                    ..Default::default()
                },
            )
            .collect::<Vec<_>>();
        let dropped = cap_daily_pending_with_limits(&mut stats, &mut domains, 3, 2, 1 << 30);
        assert_eq!(dropped, 5);
        assert_eq!(
            stats.iter().map(|row| row.server_id).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            domains.iter().map(|row| row.server_id).collect::<Vec<_>>(),
            vec![0, 1]
        );

        let mut stats = vec![pb::ServerDailyStat::default()];
        let mut domains = vec![pb::upload_server_daily_stats_request::DomainStat::default()];
        assert_eq!(
            cap_daily_pending_with_limits(&mut stats, &mut domains, 8, 8, 0),
            2
        );
        assert!(stats.is_empty());
        assert!(domains.is_empty());
    }

    #[test]
    fn trims_http_request_dimension_rows_per_server() {
        let mut req = pb::UploadServerHttpRequestStatRequest::default();
        for count in [10, 30, 20] {
            req.region_cities
                .push(pb::upload_server_http_request_stat_request::RegionCity {
                    server_id: 1,
                    region_city_id: count,
                    count_requests: count,
                    ..Default::default()
                });
            req.region_providers.push(
                pb::upload_server_http_request_stat_request::RegionProvider {
                    server_id: 1,
                    region_provider_id: count,
                    count,
                },
            );
            req.systems
                .push(pb::upload_server_http_request_stat_request::System {
                    server_id: 1,
                    name: format!("os-{count}"),
                    count,
                    ..Default::default()
                });
            req.browsers
                .push(pb::upload_server_http_request_stat_request::Browser {
                    server_id: 1,
                    name: format!("browser-{count}"),
                    count,
                    ..Default::default()
                });
        }
        req.region_cities
            .push(pb::upload_server_http_request_stat_request::RegionCity {
                server_id: 2,
                region_city_id: 5,
                count_requests: 5,
                ..Default::default()
            });

        trim_http_request_stat_rows_with_limits(
            &mut req,
            HttpRequestStatUploadLimits {
                max_cities: 2,
                max_providers: 2,
                max_systems: 2,
                max_browsers: 2,
            },
        );

        assert_eq!(
            req.region_cities
                .iter()
                .filter(|row| row.server_id == 1)
                .map(|row| row.region_city_id)
                .collect::<Vec<_>>(),
            vec![30, 20]
        );
        assert_eq!(
            req.region_cities
                .iter()
                .filter(|row| row.server_id == 2)
                .count(),
            1
        );
        assert_eq!(req.region_providers.len(), 2);
        assert_eq!(req.systems.len(), 2);
        assert_eq!(req.browsers.len(), 2);
    }

    #[test]
    fn bandwidth_pending_backlog_is_bounded_and_overflow_is_an_explicit_gap() {
        let window = |start| PendingWindow {
            window_start: start,
            segment_started_at: start,
            source_boot: "source-generation".into(),
            stats: vec![pb::ServerBandwidthStat {
                server_id: 7,
                total_bytes: 42,
                ..Default::default()
            }],
            error_counts: HashMap::new(),
        };
        let mut pending = Vec::new();
        for start in 0..MAX_PENDING_BANDWIDTH_WINDOWS {
            assert!(retain_pending_window(&mut pending, window(start as i64)).is_none());
        }
        let dropped =
            retain_pending_window(&mut pending, window(99)).expect("the cap must reject, not grow");
        assert_eq!(pending.len(), MAX_PENDING_BANDWIDTH_WINDOWS);
        let gap = drop_pending_window_gap(&dropped);
        assert!(gap.has_gap);
        assert_eq!(gap.gap_kind, "bandwidth");
        assert_eq!(gap.window_start, 99);
        assert!(gap.gap_reason.contains("12-window cap"));
    }

    #[test]
    fn bandwidth_handoff_state_keeps_only_undelivered_windows_and_source_identity() {
        let window = |start| PendingWindow {
            window_start: start,
            segment_started_at: start,
            source_boot: "source-generation".into(),
            stats: vec![pb::ServerBandwidthStat {
                server_id: 7,
                total_bytes: 42,
                ..Default::default()
            }],
            error_counts: HashMap::new(),
        };
        let pending = [window(300), window(600)];
        let batch = window_batches(&pending[0], true).remove(0);
        assert_eq!(batch.source_boot_id, "source-generation");
        assert_eq!(batch.window_start, 300);
        let raw =
            encode_bandwidth_state("next-window", &HashMap::new(), &pending[1..], 900).unwrap();
        let state: StoredBandwidthState = serde_json::from_slice(&raw).unwrap();
        assert_eq!(state.pending_windows.len(), 1);
        assert_eq!(state.pending_windows[0].window_start, 600);
        assert_eq!(state.pending_windows[0].source_boot, "source-generation");
        assert_eq!(state.pending_windows[0].stats[0].total_bytes, 42);
        assert_eq!(
            pending.len(),
            2,
            "preparing state cannot release uncommitted work"
        );
    }

    #[test]
    fn bandwidth_pending_stat_round_trips_storage_shape() {
        let stat = pb::ServerBandwidthStat {
            user_id: 1,
            server_id: 2,
            day: "20260621".to_string(),
            time_at: "1200".to_string(),
            bytes: 3,
            bits: 24,
            total_bytes: 4,
            cached_bytes: 5,
            attack_bytes: 6,
            count_requests: 7,
            count_cached_requests: 8,
            count_attack_requests: 9,
            user_plan_id: 10,
            count_websocket_connections: 11,
            origin_total_bytes: 12,
            origin_avg_bytes: 13,
            origin_avg_bits: 14,
            count_i_ps: 15,
            node_region_id: 16,
            ..Default::default()
        };

        let restored = pb::ServerBandwidthStat::from(StoredPendingBandwidthStat::from(&stat));

        assert_eq!(restored.server_id, stat.server_id);
        assert_eq!(restored.total_bytes, stat.total_bytes);
        assert_eq!(restored.count_i_ps, stat.count_i_ps);
        assert_eq!(restored.node_region_id, stat.node_region_id);
    }

    #[test]
    fn origin_health_map_gc_removes_stale_entries_only() {
        let now = crate::utils::time::now_timestamp();
        let fresh_id = i64::MAX - 1;
        let stale_id = i64::MAX - 2;
        ORIGIN_HEALTH_MAP.insert(
            fresh_id,
            Arc::new(OriginHealthEntry {
                status: AtomicU8::new(ORIGIN_HEALTH_STATUS_HEALTHY),
                latency_ms: AtomicI64::new(1),
                last_check_ts: AtomicI64::new(now),
            }),
        );
        ORIGIN_HEALTH_MAP.insert(
            stale_id,
            Arc::new(OriginHealthEntry {
                status: AtomicU8::new(ORIGIN_HEALTH_STATUS_DOWN),
                latency_ms: AtomicI64::new(1),
                last_check_ts: AtomicI64::new(now - ORIGIN_HEALTH_STALE_SECS - 1),
            }),
        );
        gc_stale_origin_health(now);
        assert!(ORIGIN_HEALTH_MAP.contains_key(&fresh_id));
        assert!(!ORIGIN_HEALTH_MAP.contains_key(&stale_id));
        ORIGIN_HEALTH_MAP.remove(&fresh_id);
        ORIGIN_HEALTH_MAP.remove(&stale_id);
    }

    #[test]
    fn origin_health_map_respects_capacity_bound() {
        // cap = 0 makes any new insert deterministically refused, immune to
        // concurrent tests mutating the shared map's length.
        let id_a = i64::MAX - 1_000_010;
        let id_c = i64::MAX - 1_000_012;
        push_origin_health_event_with_capacity(id_a, true, 1, usize::MAX);
        assert!(ORIGIN_HEALTH_MAP.contains_key(&id_a));
        // At capacity a brand-new id is refused; existing ids still update.
        push_origin_health_event_with_capacity(id_c, true, 1, 0);
        assert!(!ORIGIN_HEALTH_MAP.contains_key(&id_c));
        push_origin_health_event_with_capacity(id_a, false, 9, 0);
        let entry = ORIGIN_HEALTH_MAP.get(&id_a).expect("existing entry");
        assert_eq!(
            entry.status.load(Ordering::Relaxed),
            ORIGIN_HEALTH_STATUS_DOWN
        );
        drop(entry);
        ORIGIN_HEALTH_MAP.remove(&id_a);
        ORIGIN_HEALTH_MAP.remove(&id_c);
    }

    fn test_now(minute: u32) -> DateTime<FixedOffset> {
        use chrono::TimeZone as _;
        FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 31, 12, minute, 0)
            .unwrap()
    }

    struct RecordingMetricSink {
        uploaded: Vec<pb::UploadMetricStatsRequest>,
    }

    impl MetricUploadSink for RecordingMetricSink {
        async fn send(&mut self, request: &pb::UploadMetricStatsRequest) -> anyhow::Result<()> {
            self.uploaded.push(request.clone());
            Ok(())
        }
    }

    struct RefusingMetricSink {
        calls: usize,
    }

    impl MetricUploadSink for RefusingMetricSink {
        async fn send(&mut self, _request: &pb::UploadMetricStatsRequest) -> anyhow::Result<()> {
            self.calls += 1;
            Err(anyhow::anyhow!("spool unavailable"))
        }
    }

    #[test]
    fn snapshot_window_end_uses_the_last_real_event_not_the_future_period() {
        assert_eq!(
            snapshot_window_end(1_790_000_060, 1_790_000_000, 0),
            1_790_000_060
        );
        assert_eq!(
            snapshot_window_end(1_790_000_060, 1_790_000_000, 1_790_000_012_900),
            1_790_000_013
        );
        // A period end in the past still bounds the window.
        assert_eq!(
            snapshot_window_end(1_790_000_060, 1_790_000_000, 1_790_000_090_000),
            1_790_000_060
        );
    }

    #[test]
    fn cumulative_snapshot_values_add_deltas_and_never_use_keep_keys() {
        let dir =
            std::env::temp_dir().join(format!("zelvik-stats-snapshot-{}", uuid::Uuid::new_v4()));
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = snapshot_key(1, 7, &["country=FR"]);
        let merge = state.merge(vec![
            crate::rpc::metric_stat_cache::SnapshotDelta {
                key: identity.clone(),
                window_end: 1_790_000_030,
                value: 10.0,
                charge: 0,
            },
            crate::rpc::metric_stat_cache::SnapshotDelta {
                key: identity.clone(),
                window_end: 1_790_000_031,
                value: 50.0,
                charge: 0,
            },
        ]);
        assert_eq!(merge.updated, 1);
        assert_eq!(state.row_value(&identity), Some(60.0));
        let rows = state.dirty_rows();
        let (chunks, quarantine) =
            build_snapshot_chunks(rows, state.epoch(), metric_stat_payload_budget());
        assert!(quarantine.is_empty());
        assert_eq!(chunks.len(), 1);
        let request = &chunks[0].request;
        assert_eq!(request.metric_stats.len(), 1);
        assert_eq!(request.metric_stats[0].value, 60.0);
        assert!(request.keep_keys.is_empty());
        assert!(request.revision > 0);
        assert_eq!(request.window_start, 1_790_000_000);
        assert_eq!(request.window_end, 1_790_000_031);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_rows_for_two_servers_stay_separate_before_the_receiver_sums_them() {
        let dir =
            std::env::temp_dir().join(format!("zelvik-stats-servers-{}", uuid::Uuid::new_v4()));
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let first = snapshot_key(1, 10, &["country=FR"]);
        let second = snapshot_key(1, 20, &["country=FR"]);
        state.merge(vec![
            crate::rpc::metric_stat_cache::SnapshotDelta {
                key: first,
                window_end: 1_790_000_030,
                value: 10.0,
                charge: 0,
            },
            crate::rpc::metric_stat_cache::SnapshotDelta {
                key: second,
                window_end: 1_790_000_030,
                value: 20.0,
                charge: 0,
            },
        ]);
        let (chunks, _) = build_snapshot_chunks(
            state.dirty_rows(),
            state.epoch(),
            metric_stat_payload_budget(),
        );
        assert_eq!(chunks.len(), 2);
        assert_ne!(chunks[0].request.server_id, chunks[1].request.server_id);
        let total: f32 = chunks.iter().map(|chunk| chunk.request.total).sum();
        assert_eq!(total, 30.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_hash_uses_the_row_incarnation_not_the_request_revision() {
        let identity = snapshot_key(1, 7, &["country=FR"]);
        // Same key and same request revision, different row instance: the wire
        // hash must differ so the CP keeps both cumulative instances.
        let mut old_instance = dirty(identity.clone(), 10.0, 1);
        old_instance.incarnation = 1;
        let mut new_instance = dirty(identity.clone(), 5.0, 1);
        new_instance.incarnation = 2;

        let hash = |row: DirtySnapshotRow| {
            build_snapshot_chunks(vec![row], "epoch", usize::MAX).0[0]
                .request
                .metric_stats[0]
                .hash
                .clone()
        };
        let old_hash = hash(old_instance.clone());
        assert_eq!(hash(old_instance), old_hash);
        assert_ne!(hash(new_instance), old_hash);
    }

    #[test]
    fn oversized_snapshot_records_are_quarantined_not_dropped() {
        let identity = snapshot_key(1, 7, &["key=very-long"]);
        let rows = vec![dirty(identity, 1.0, 1)];
        // A header plus one record cannot fit this budget, so the exact row is
        // returned for the durable quarantine path.
        let budget = 1;
        let (chunks, quarantine) = build_snapshot_chunks(rows, "epoch", budget);
        assert!(chunks.is_empty());
        assert_eq!(quarantine.len(), 1);
        assert_eq!(quarantine[0].request.metric_stats.len(), 1);
        assert_eq!(quarantine[0].rows.len(), 1);
    }

    #[test]
    fn snapshot_split_preserves_every_row_and_header() {
        let rows: Vec<_> = (0..40)
            .map(|index| {
                dirty(
                    snapshot_key(1, 7, &[&format!("city=Paris-{index}")]),
                    index as f32,
                    1,
                )
            })
            .collect();
        let total_rows = rows.len();
        let header = build_snapshot_chunks(vec![rows[0].clone()], "epoch", usize::MAX)
            .0
            .pop()
            .unwrap()
            .request;
        let budget = prost::Message::encoded_len(&header) + 300;
        let (chunks, quarantine) = build_snapshot_chunks(rows, "epoch", budget);
        assert!(quarantine.is_empty());
        assert!(chunks.len() > 1);
        let uploaded: usize = chunks
            .iter()
            .map(|chunk| chunk.request.metric_stats.len())
            .sum();
        assert_eq!(uploaded, total_rows);
        for chunk in &chunks {
            assert_eq!(chunk.request.revision, 1);
            assert_eq!(chunk.request.window_start, 1_790_000_000);
            assert_eq!(chunk.request.window_end, 1_790_000_030);
            assert!(chunk.request.keep_keys.is_empty());
        }
    }

    #[tokio::test]
    async fn tick_uploads_dirty_snapshots_and_checkpoints_uploaded_revision() {
        let dir = std::env::temp_dir().join(format!("zelvik-stats-tick-{}", uuid::Uuid::new_v4()));
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = snapshot_key(1, 7, &["country=FR"]);
        state.merge(vec![crate::rpc::metric_stat_cache::SnapshotDelta {
            key: identity.clone(),
            window_end: 1_790_000_030,
            value: 10.0,
            charge: 0,
        }]);
        let mut sink = RecordingMetricSink { uploaded: vec![] };
        let outcome = run_metric_stat_tick(
            &mut state,
            &[metric_item(1, vec!["${country}"], "${countRequest}")],
            &[],
            &test_now(30),
            &mut sink,
        )
        .await;
        assert_eq!(outcome.enqueued, 1);
        assert_eq!(sink.uploaded.len(), 1);
        assert!(sink.uploaded[0].revision > 0);
        assert_eq!(state.dirty_len(), 0);

        // A restart must see the uploaded marker and the same monotonic epoch.
        let epoch = state.epoch().to_string();
        drop(state);
        let resumed = MetricStatReporterState::open(&dir).unwrap();
        assert_eq!(resumed.epoch(), epoch);
        assert_eq!(resumed.dirty_len(), 0);
        assert_eq!(resumed.row_value(&identity), Some(10.0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn failed_spool_keeps_snapshot_dirty_for_a_same_revision_retry() {
        let dir = std::env::temp_dir().join(format!("zelvik-stats-retry-{}", uuid::Uuid::new_v4()));
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = snapshot_key(1, 7, &["country=FR"]);
        state.merge(vec![crate::rpc::metric_stat_cache::SnapshotDelta {
            key: identity.clone(),
            window_end: 1_790_000_030,
            value: 10.0,
            charge: 0,
        }]);
        let revision = state.row_revision(&identity).unwrap();
        let mut refusing = RefusingMetricSink { calls: 0 };
        let first = run_metric_stat_tick(&mut state, &[], &[], &test_now(30), &mut refusing).await;
        assert_eq!(first.deferred, 1);
        assert_eq!(state.dirty_len(), 1);

        let mut recording = RecordingMetricSink { uploaded: vec![] };
        let second =
            run_metric_stat_tick(&mut state, &[], &[], &test_now(30), &mut recording).await;
        assert_eq!(second.enqueued, 1);
        assert_eq!(recording.uploaded[0].revision, revision);
        assert_eq!(state.dirty_len(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn checkpoint_failure_blocks_upload_and_keeps_rows_dirty() {
        let dir = std::env::temp_dir().join(format!(
            "zelvik-stats-checkpoint-fail-{}",
            uuid::Uuid::new_v4()
        ));
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        state.merge(vec![crate::rpc::metric_stat_cache::SnapshotDelta {
            key: snapshot_key(1, 7, &["country=FR"]),
            window_end: 1_790_000_030,
            value: 10.0,
            charge: 0,
        }]);
        // Force the encoded checkpoint above its cap.
        state.set_max_checkpoint_bytes(1);
        let mut sink = RecordingMetricSink { uploaded: vec![] };
        let outcome = run_metric_stat_tick(&mut state, &[], &[], &test_now(30), &mut sink).await;
        assert!(sink.uploaded.is_empty());
        assert_eq!(outcome.deferred, 1);
        assert!(state.stats().backpressured);
        assert_eq!(state.dirty_len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
