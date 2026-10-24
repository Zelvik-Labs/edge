use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::metrics::ServerStatusSnapshot;
use crate::pb;
use crate::rpc::client::SharedRpcClient;
use crate::rpc::metric_stat_cache::{
    MetricScope, MetricStatReporterState, MetricValuesCache, MetricValuesCacheStats,
    PendingMetricUpload,
};
use chrono::{DateTime, Datelike, Duration as ChronoDuration, FixedOffset, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use tracing::{debug, info, warn};

#[allow(dead_code)]
const ORIGIN_HEALTH_STATUS_UNKNOWN: u8 = 0;
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

    let now = crate::utils::time::now_local();
    let today = now.format("%Y%m%d").to_string();
    let available_time = (now - ChronoDuration::seconds(BANDWIDTH_REPORT_INTERVAL_SECS as i64))
        .format("%H%M")
        .to_string();
    let mut window_stats = HashMap::new();
    for stat in state.window_stats {
        if stat.day == today && stat.time_at >= available_time {
            window_stats.insert(stat.server_id, stat);
        }
    }
    let current_window = if window_stats.is_empty() {
        String::new()
    } else {
        state.current_window
    };

    // Legacy `pending_stats` (pre-spool state) is dropped: its entries were
    // either already uploaded or their windows surface as incomplete.
    let pending_windows = state
        .pending_windows
        .into_iter()
        .map(PendingWindow::from)
        .collect();

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
    match serde_json::to_vec(&state) {
        Ok(raw) => {
            if let Err(e) = crate::telemetry::save_state(BANDWIDTH_STATE_STORAGE_KEY, raw).await {
                warn!("bandwidth window state persist failed: {}", e);
            }
        }
        Err(e) => warn!("bandwidth window state encode failed: {}", e),
    }
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

async fn enqueue_window_batch(window: &PendingWindow, complete: bool) -> anyhow::Result<()> {
    let request = pb::UploadServerBandwidthStatsRequest {
        server_bandwidth_stats: window.stats.clone(),
    };
    let mut batch = pb::TelemetryBatch {
        kind: "bandwidth".into(),
        payload: prost::Message::encode_to_vec(&request),
        occurred_at: crate::utils::time::now_timestamp(),
        window_start: window.window_start,
        window_complete: complete,
        error_counts: window.error_counts.clone(),
        segment_started_at: window.segment_started_at,
        ..Default::default()
    };
    if !window.source_boot.is_empty() {
        batch.source_boot_id = window.source_boot.clone();
    }
    crate::telemetry::enqueue_prepared(batch).await
}

/// A window that closed while the process was down keeps partial coverage:
/// upload the accumulation as its own boot segment (never a completion
/// marker) and flag the downtime remainder as unrecoverable.
async fn flush_stale_window(
    config_store: &ConfigStore,
    current_window: &mut String,
    window_stats: &mut HashMap<i64, BandwidthWindowStat>,
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
    if let Err(e) = enqueue_window_batch(&window, false).await {
        warn!(
            "stale bandwidth window could not reach the durable queue: {}",
            e
        );
    }
    // The downtime span is permanent loss: emit a gap batch whose range covers
    // the stale window so settlement for it stays paused.
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
    let mut last_snapshots: HashMap<i64, ServerStatusSnapshot> = HashMap::new();
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
        &stored_boot,
        segment_started_at,
    )
    .await;

    loop {
        interval.tick().await;
        let snapshots = crate::metrics::METRICS.take_snapshots();

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
            pending_windows.push(drain_bandwidth_window(
                &config_store,
                &mut window_stats,
                closed_start,
                segment_started_at,
                node_region_id,
                String::new(),
            ));
            let mut sent = 0;
            while sent < pending_windows.len() {
                if enqueue_window_batch(&pending_windows[sent], true)
                    .await
                    .is_err()
                {
                    break;
                }
                sent += 1;
            }
            pending_windows.drain(..sent);
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

        for snap_tuple in snapshots {
            let snap = &snap_tuple.1;
            let prior = last_snapshots.insert(snap.server_id, snap.clone());
            // Skip first sample for a server — without a baseline the delta
            // would be the cumulative total, producing a phantom bandwidth spike.
            if prior.is_none() {
                continue;
            }
            let delta = snapshot_delta(snap, prior.as_ref());
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
        persist_bandwidth_state(
            &current_window,
            &window_stats,
            &pending_windows,
            segment_started_at,
        )
        .await;
    }
}

pub async fn start_daily_stat_reporter(config_store: ConfigStore) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
        BANDWIDTH_SAMPLE_INTERVAL_SECS,
    ));
    let mut last_snapshots: HashMap<i64, ServerStatusSnapshot> = HashMap::new();
    let mut current_window = String::new();
    let mut window_stats: HashMap<i64, pb::ServerDailyStat> = HashMap::new();
    let mut pending_stats: Vec<pb::ServerDailyStat> = Vec::new();
    let mut pending_domain_stats: Vec<pb::upload_server_daily_stats_request::DomainStat> =
        Vec::new();

    loop {
        interval.tick().await;
        let snapshots = crate::metrics::METRICS.take_snapshots();

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
                    stats: stats.clone(),
                    domain_stats: domain_stats.clone(),
                };
                // A full spool keeps the drained window in memory — pending
                // entries are never aged out, they are re-offered next window.
                if let Err(e) = crate::telemetry::enqueue("daily", &request, 0, false).await {
                    warn!("Failed to persist daily stats: {}", e);
                    pending_stats = stats;
                    pending_domain_stats = domain_stats;
                }
            }
            current_window = window_key.clone();
        }

        for snap_tuple in snapshots {
            let snap = &snap_tuple.1;
            let prior = last_snapshots.insert(snap.server_id, snap.clone());
            // Skip first sample for a server — without a baseline the delta
            // would be the cumulative total, producing phantom traffic.
            if prior.is_none() {
                continue;
            }
            let delta = snapshot_delta(snap, prior.as_ref());
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
        }
    }
}

/// Window key for a metric item. The caller injects the clock so every helper
/// in one tick — scope pruning, request building and cache commits — agrees on
/// the same instant, and tests can move it backwards deterministically.
fn get_period_time_at(period: i32, unit: &str, now: &DateTime<FixedOffset>) -> String {
    match unit.to_lowercase().as_str() {
        "month" => now.format("%Y%m").to_string(),
        "week" => {
            let week = now.iso_week();
            format!("{:04}{:02}", week.year(), week.week())
        }
        "day" => now.format("%Y%m%d").to_string(),
        "hour" => {
            if period > 1 {
                let hour = (now.hour() as i32 / period) * period;
                format!("{}{:02}", now.format("%Y%m%d"), hour)
            } else {
                now.format("%Y%m%d%H").to_string()
            }
        }
        "minute" => {
            if period > 1 {
                let minute = (now.minute() as i32 / period) * period;
                format!("{}{:02}", now.format("%Y%m%d%H"), minute)
            } else {
                now.format("%Y%m%d%H%M").to_string()
            }
        }
        _ => now.format("%Y%m%d%H%M").to_string(),
    }
}

fn metric_item_category(item: &crate::config_models::MetricItemConfig) -> String {
    crate::metrics::normalize_metric_category(&item.category)
}

fn sample_matches_metric_category(
    sample: &(
        crate::metrics::aggregator::AggregationKey,
        crate::metrics::aggregator::AggregatedValue,
    ),
    category: &str,
) -> bool {
    sample.0.category.as_ref() == category
}

fn metric_sample_value(
    item_value: &serde_json::Value,
    value: &crate::metrics::aggregator::AggregatedRequestValue,
) -> f32 {
    match item_value.as_str() {
        Some("${bytesSent}") | Some("${countTrafficOut}") => value.bytes_sent as f32,
        Some("${countTrafficIn}") => value.bytes_received as f32,
        Some("${countRequest}") => value.count as f32,
        Some("${countConnection}") => value.count as f32,
        Some("${countAttackRequest}") => value.count_attack as f32,
        _ => value.count as f32,
    }
}

fn build_metric_uploads_for_item(
    item: &crate::config_models::MetricItemConfig,
    time_key: &str,
    samples: &[(
        crate::metrics::aggregator::AggregationKey,
        crate::metrics::aggregator::AggregatedValue,
    )],
    values_cache: &mut MetricValuesCache,
) -> Vec<pb::UploadMetricStatsRequest> {
    let item_category = metric_item_category(item);
    let mut grouped: HashMap<(i64, Vec<String>), (i64, f32)> = HashMap::new();

    for (key, value) in samples
        .iter()
        .filter(|sample| sample_matches_metric_category(sample, &item_category))
    {
        if value.request_samples.is_empty() {
            let aggregate_value = crate::metrics::aggregator::AggregatedRequestValue {
                count: value.count,
                count_attack: value.count_attack,
                bytes_sent: value.bytes_sent,
                bytes_received: value.bytes_received,
                attack_bytes: value.attack_bytes,
            };
            let keys = item
                .keys
                .iter()
                .map(|configured_key| key.resolve_metric_key(configured_key))
                .collect::<Vec<_>>();
            let entry = grouped.entry((key.server_id, keys)).or_default();
            entry.0 += aggregate_value.count;
            entry.1 += metric_sample_value(&item.value, &aggregate_value);
            continue;
        }

        for (request_attrs, request_value) in &value.request_samples {
            let keys = item
                .keys
                .iter()
                .map(|configured_key| {
                    key.resolve_metric_key_with_attrs(configured_key, request_attrs.as_ref())
                })
                .collect::<Vec<_>>();
            let entry = grouped.entry((key.server_id, keys)).or_default();
            entry.0 += request_value.count;
            entry.1 += metric_sample_value(&item.value, request_value);
        }
    }

    let mut by_server: HashMap<i64, Vec<(Vec<String>, i64, f32)>> = HashMap::new();
    for ((server_id, keys), (count, value)) in grouped {
        by_server
            .entry(server_id)
            .or_default()
            .push((keys, count, value));
    }

    let mut requests = Vec::with_capacity(by_server.len());
    for (server_id, mut rows) in by_server {
        rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut metric_stats = Vec::with_capacity(rows.len());
        let mut keep_keys = Vec::new();
        let mut count = 0i64;
        let mut total = 0f32;

        for (keys, row_count, value) in rows {
            count += row_count;
            total += value;

            // GO ALGORITHM: hashString(serverId + "@" + keys.join("$EDGE$") + "@" + time + "@" + version + "@" + itemId)
            let keys_data = keys.join("$EDGE$");
            let hash_raw = format!(
                "{}@{}@{}@{}@{}",
                server_id, keys_data, time_key, item.version, item.id
            );
            let hash = crate::utils::fnv_hash64(&hash_raw).to_string();

            // The cache is read-only here: a value is committed only after its
            // payload is durable in the spool. A miss (new, evicted, expired or
            // wrong-version) sends the full row again — never a dropped sample.
            if let Some(old_val) = values_cache.lookup(item.id, item.version, time_key, &hash)
                && (value - old_val).abs() < 0.001
            {
                keep_keys.push(hash);
                continue;
            }

            metric_stats.push(pb::UploadingMetricStat {
                id: 0,
                hash,
                keys,
                value,
            });
        }

        requests.push(pb::UploadMetricStatsRequest {
            server_id,
            time: time_key.to_string(),
            count,
            total,
            version: item.version,
            item_id: item.id,
            metric_stats,
            keep_keys,
        });
    }

    requests
}

type MetricStatSample = (
    crate::metrics::aggregator::AggregationKey,
    crate::metrics::aggregator::AggregatedValue,
);

/// Enqueue seam. The reporter and its tests both drive the same split/retry
/// logic; only the durable sink differs.
trait MetricUploadSink {
    fn send(
        &mut self,
        request: &pb::UploadMetricStatsRequest,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;
}

struct TelemetryMetricUploadSink;

impl MetricUploadSink for TelemetryMetricUploadSink {
    fn send(
        &mut self,
        request: &pb::UploadMetricStatsRequest,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
        async move { crate::telemetry::enqueue("metric_stat", request, 0, false).await }
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
    // The payload field's own tag plus a length varint (at most five bytes).
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

struct MetricRequestSplit {
    chunks: Vec<pb::UploadMetricStatsRequest>,
    /// Requests carrying exactly one record that cannot fit the wire even
    /// alone. They are handed to the durable local quarantine, never dropped.
    quarantine: Vec<pb::UploadMetricStatsRequest>,
}

/// Split one per-item/per-server request into chunks whose serialized payload
/// stays inside `budget`.
///
/// `count` and `total` are request-level aggregates: every row in the original
/// request carried the same pair, so every chunk keeps the original pair. Rows
/// are moved, never copied or dropped; a row that cannot fit even alone keeps
/// its full record inside a single-record request in `quarantine`.
fn split_metric_stat_request(
    request: pb::UploadMetricStatsRequest,
    budget: usize,
) -> MetricRequestSplit {
    let pb::UploadMetricStatsRequest {
        server_id,
        time,
        count,
        total,
        version,
        item_id,
        metric_stats,
        keep_keys,
    } = request;

    let header = || pb::UploadMetricStatsRequest {
        server_id,
        time: time.clone(),
        count,
        total,
        version,
        item_id,
        metric_stats: Vec::new(),
        keep_keys: Vec::new(),
    };
    let header_cost = prost::Message::encoded_len(&header());

    let mut chunks = Vec::new();
    let mut quarantine = Vec::new();
    let mut current = header();
    let mut used = header_cost;
    let mut current_has_records = false;

    for stat in metric_stats {
        let cost = prost::encoding::message::encoded_len(7, &stat);
        if header_cost.saturating_add(cost) > budget {
            let mut single = header();
            single.metric_stats.push(stat);
            quarantine.push(single);
            continue;
        }
        if used.saturating_add(cost) > budget && current_has_records {
            chunks.push(std::mem::replace(&mut current, header()));
            used = header_cost;
        }
        used = used.saturating_add(cost);
        current.metric_stats.push(stat);
        current_has_records = true;
    }

    for hash in keep_keys {
        let cost = prost::encoding::string::encoded_len(8, &hash);
        if header_cost.saturating_add(cost) > budget {
            // A keep key is a generated FNV hash in practice and cannot reach
            // this branch; if a future producer changes that, preserve it in
            // quarantine rather than dropping the marker.
            let mut single = header();
            single.keep_keys.push(hash);
            quarantine.push(single);
            continue;
        }
        if used.saturating_add(cost) > budget && current_has_records {
            chunks.push(std::mem::replace(&mut current, header()));
            used = header_cost;
        }
        used = used.saturating_add(cost);
        current.keep_keys.push(hash);
        current_has_records = true;
    }

    if current_has_records {
        chunks.push(current);
    }
    MetricRequestSplit { chunks, quarantine }
}

/// Only enabled items are live. Disabling or deleting an item, bumping its
/// version or moving into a new window therefore makes its old cache entries
/// stale, and the next tick prunes them.
fn metric_values_scope(
    metric_items: &[crate::config_models::MetricItemConfig],
    now: &DateTime<FixedOffset>,
) -> MetricScope {
    MetricScope::from_windows(metric_items.iter().filter(|item| item.is_on).map(|item| {
        (
            item.id,
            item.version,
            get_period_time_at(item.period, &item.period_unit, now),
        )
    }))
}

/// A value becomes deduplicatable only after its exact payload is durable.
/// Commits for a window/version that is no longer live are ignored.
fn commit_metric_upload(
    cache: &mut MetricValuesCache,
    scope: &MetricScope,
    request: &pb::UploadMetricStatsRequest,
) {
    for stat in &request.metric_stats {
        cache.commit(
            scope,
            request.item_id,
            request.version,
            &request.time,
            &stat.hash,
            stat.value,
        );
    }
}

#[derive(Default)]
struct MetricStatTickOutcome {
    pruned: usize,
    enqueued: usize,
    deferred: usize,
}

/// Re-offer retained payloads oldest first, per item.
///
/// Items are independent: a failure on one item's head stops only that item's
/// queue, so every other item keeps uploading. There is no retry cap and no
/// drop branch — a payload leaves these queues only after a durable spool
/// append or a durable local quarantine write.
async fn retry_pending_metric_uploads<S: MetricUploadSink>(
    state: &mut MetricStatReporterState,
    scope: &MetricScope,
    now: &DateTime<FixedOffset>,
    sink: &mut S,
) -> (usize, usize) {
    let mut enqueued = 0;
    let mut quarantined = 0;
    for item_id in state.pending_item_ids() {
        while let Some(pending) = state.pop_pending(item_id) {
            if pending.quarantine_only {
                match state.store_quarantine(&pending.request).await {
                    Ok(bytes) => {
                        // The gap is best-effort upstream; the payload itself is
                        // already fsynced locally before this point.
                        crate::telemetry::record_gap(
                            "metric_stat",
                            now.timestamp(),
                            "metric_stat_record_over_batch_cap",
                        );
                        info!(
                            item_id,
                            server_id = pending.request.server_id,
                            bytes,
                            dir = %state.quarantine_dir().display(),
                            "metric stat record durably quarantined locally"
                        );
                        state.discard_pending_file(&pending);
                        quarantined += 1;
                    }
                    Err(e) => {
                        state.record_retry_refusal();
                        warn!(
                            item_id,
                            server_id = pending.request.server_id,
                            bytes = pending.encoded_bytes,
                            "local metric stat quarantine refused the record; payload retained: {e}"
                        );
                        state.return_pending_front(item_id, pending);
                        break;
                    }
                }
                continue;
            }

            match sink.send(&pending.request).await {
                Ok(()) => {
                    commit_metric_upload(state.cache_mut(), scope, &pending.request);
                    state.discard_pending_file(&pending);
                    enqueued += 1;
                }
                Err(e) if crate::telemetry::is_batch_too_large(e.as_ref()) => {
                    // The split budget should make this unreachable. If the
                    // seam ever drifts, keep every byte and move the exact
                    // request to the durable quarantine path instead of
                    // retrying the wire forever or dropping it.
                    warn!(
                        item_id,
                        server_id = pending.request.server_id,
                        "metric stats payload exceeded the wire at the spool; retaining for durable quarantine: {e}"
                    );
                    let pending = PendingMetricUpload {
                        quarantine_only: true,
                        ..pending
                    };
                    state.return_pending_front(item_id, pending);
                    break;
                }
                Err(e) => {
                    if !crate::telemetry::is_capacity_exhausted(e.as_ref()) {
                        state.record_retry_refusal();
                    }
                    warn!(
                        item_id,
                        server_id = pending.request.server_id,
                        bytes = pending.encoded_bytes,
                        "metric stats upload failed; exact payload retained at its item head: {e}"
                    );
                    state.return_pending_front(item_id, pending);
                    break;
                }
            }
        }
    }
    (enqueued, quarantined)
}

/// Build, split and retain one tick's uploads.
///
/// Every request is split before it reaches the spool so no chunk can be
/// rejected as a whole. A chunk is committed only after its own enqueue is
/// durable. A failed chunk and every later chunk of the same item are appended
/// to that item's queue in order; other items are not blocked. Nothing is
/// restored into the aggregator on partial success, so already-enqueued chunks
/// cannot be re-aggregated and duplicated by the next batch.
async fn dispatch_metric_uploads<S: MetricUploadSink>(
    state: &mut MetricStatReporterState,
    scope: &MetricScope,
    metric_items: &[crate::config_models::MetricItemConfig],
    samples: &[MetricStatSample],
    now: &DateTime<FixedOffset>,
    sink: &mut S,
) -> (usize, usize) {
    let budget = metric_stat_payload_budget();
    let mut enqueued = 0;
    let mut deferred = 0;
    for item in metric_items.iter().filter(|item| item.is_on) {
        let item_category = metric_item_category(item);
        if !samples
            .iter()
            .any(|sample| sample_matches_metric_category(sample, &item_category))
        {
            continue;
        }

        let time_key = get_period_time_at(item.period, &item.period_unit, now);
        let requests = build_metric_uploads_for_item(item, &time_key, samples, state.cache_mut());
        for request in requests {
            let split = split_metric_stat_request(request, budget);
            for request in split.quarantine {
                let pending = state
                    .persist_pending(PendingMetricUpload::for_quarantine(request))
                    .await;
                debug!(
                    item_id = item.id,
                    bytes = pending.encoded_bytes,
                    "metric stat record retained for the durable local quarantine"
                );
                state.push_pending(item.id, pending);
                deferred += 1;
            }
            for chunk in split.chunks {
                if state.has_pending(item.id) {
                    let pending = state
                        .persist_pending(PendingMetricUpload::for_spool(chunk))
                        .await;
                    state.push_pending(item.id, pending);
                    deferred += 1;
                    continue;
                }
                match sink.send(&chunk).await {
                    Ok(()) => {
                        commit_metric_upload(state.cache_mut(), scope, &chunk);
                        enqueued += 1;
                    }
                    Err(e) => {
                        warn!(
                            "Failed to persist metric stats for server {} item {}: {} (exact payload retained at its item head)",
                            chunk.server_id, chunk.item_id, e
                        );
                        let pending = state
                            .persist_pending(PendingMetricUpload::for_spool(chunk))
                            .await;
                        state.push_pending(item.id, pending);
                        deferred += 1;
                    }
                }
            }
        }
    }
    (enqueued, deferred)
}

async fn run_metric_stat_tick<Flush, S>(
    state: &mut MetricStatReporterState,
    metric_items: &[crate::config_models::MetricItemConfig],
    flush_samples: Flush,
    now: &DateTime<FixedOffset>,
    sink: &mut S,
) -> MetricStatTickOutcome
where
    Flush: FnOnce() -> Option<Vec<MetricStatSample>>,
    S: MetricUploadSink,
{
    let scope = metric_values_scope(metric_items, now);
    // Prune before any early return: idle nodes and nodes with no samples must
    // still forget historical windows.
    let pruned = state.prune(&scope);
    let (enqueued, _quarantined) = retry_pending_metric_uploads(state, &scope, now, sink).await;
    let mut outcome = MetricStatTickOutcome {
        pruned,
        enqueued,
        deferred: 0,
    };

    // Backpressure is observable and lossless: while retained payloads reached
    // the ceiling, this tick's samples stay in the aggregator and are
    // dispatched by a later tick. Retry above already ran, so a drain in this
    // tick immediately lifts the gate.
    let backpressured = state.pending_bytes() >= state.max_pending_bytes();
    state.set_backpressured(backpressured);
    if backpressured {
        warn!(
            pending_bytes = state.pending_bytes(),
            max_pending_bytes = state.max_pending_bytes(),
            pending_payloads = state.pending_len(),
            "metric stat backpressure: samples retained in the aggregator, nothing dropped"
        );
    } else if let Some(samples) = flush_samples()
        && !samples.is_empty()
    {
        let (enqueued, deferred) =
            dispatch_metric_uploads(state, &scope, metric_items, &samples, now, sink).await;
        outcome.enqueued += enqueued;
        outcome.deferred = deferred;
    }

    state.publish_stats();
    outcome
}

/// Current size of the metric dedup cache. Exposed for memory governance and
/// node observability; values are the reporter's own counters.
pub fn metric_values_cache_stats() -> MetricValuesCacheStats {
    crate::rpc::metric_stat_cache::metric_values_cache_stats()
}

pub async fn start_metric_stat_reporter(config_store: Arc<crate::config::ConfigStore>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let mut state = MetricStatReporterState::with_quarantine_dir(
        crate::paths::NodePaths::current()
            .data_dir()
            .join("metric_stat_quarantine"),
    );
    let mut sink = TelemetryMetricUploadSink;
    let recovered = state.recover_pending();
    if recovered > 0 {
        info!(
            recovered,
            pending = state.pending_len(),
            dir = %state.pending_dir().display(),
            "recovered metric stat pending payloads after restart"
        );
    }

    loop {
        interval.tick().await;
        let now = crate::utils::time::now_local();
        let metric_items = config_store.get_metric_items().await;

        let pending_before = state.pending_bytes();
        let max_pending = state.max_pending_bytes();
        let outcome = run_metric_stat_tick(
            &mut state,
            &metric_items,
            || {
                // Preserve the previous sample lifecycle: an empty item list
                // leaves samples in the aggregator, while a non-empty list
                // flushes them even if every item is disabled.
                if metric_items.is_empty() {
                    return None;
                }
                // When a retry queue already exists, do not pull an aggregation
                // larger than the remaining pending budget out of the
                // aggregator: leave it there and try again once the queue
                // drains. This bounds a failure tick to roughly the budget,
                // and is a pause, never a drop.
                if pending_before > 0 {
                    let aggregator_bytes =
                        crate::metrics::aggregator::METRIC_STAT_AGGREGATOR.approximate_bytes();
                    if pending_before.saturating_add(aggregator_bytes) > max_pending {
                        debug!(
                            pending_before,
                            aggregator_bytes,
                            max_pending,
                            "metric stat flush deferred: aggregation exceeds remaining pending budget"
                        );
                        return None;
                    }
                }
                let samples = crate::metrics::aggregator::METRIC_STAT_AGGREGATOR.flush();
                (!samples.is_empty()).then_some(samples)
            },
            &now,
            &mut sink,
        )
        .await;

        if outcome.pruned > 0 {
            debug!(
                pruned = outcome.pruned,
                entries = state.stats().entries,
                "metric values cache pruned stale windows"
            );
        }
        if outcome.deferred > 0 {
            warn!(
                deferred = outcome.deferred,
                pending = state.pending_len(),
                "metric stat upload could not reach the durable spool; exact payload retained"
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

pub async fn start_metrics_aggregator_reporter(config_store: ConfigStore, api_config: ApiConfig) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));

    loop {
        interval.tick().await;

        let client = match SharedRpcClient::get(&api_config).await {
            Ok(shared) => shared.as_rpc_client(),
            Err(e) => {
                warn!("Metrics aggregator reporter failed to connect: {}", e);
                continue;
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

        let samples = crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.flush();

        if samples.is_empty() {
            continue;
        }

        let now = crate::utils::time::now_local();
        let month = now.format("%Y%m").to_string();
        let day = now.format("%Y%m%d").to_string();

        let mut req = pb::UploadServerHttpRequestStatRequest {
            month,
            day,
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

        for (key, val) in &samples {
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

        let upload_limits =
            HttpRequestStatUploadLimits::from_config(&config_store.get_global_stat_upload_sync());
        trim_http_request_stat_rows_with_limits(&mut req, upload_limits);

        if let Err(e) = crate::telemetry::enqueue("request_stat", &req, 0, false).await {
            warn!("Failed to persist HTTP request stats: {}", e);
            crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.restore(samples);
        }
    }
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
            warn!("Failed to persist top IP stats: {}", e);
            crate::metrics::top_ip::TOP_IP_TRACKER.restore(&rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric_key_with_attrs(
        request_attrs: std::collections::BTreeMap<String, String>,
    ) -> crate::metrics::aggregator::AggregationKey {
        crate::metrics::aggregator::AggregationKey {
            category: std::sync::Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: std::sync::Arc::from(""),
            country_id: 0,
            province: std::sync::Arc::from(""),
            province_id: 0,
            city: std::sync::Arc::from(""),
            city_id: 0,
            provider: std::sync::Arc::from("Unknown"),
            browser: std::sync::Arc::from(""),
            os: std::sync::Arc::from(""),
            waf_group_id: 0,
            waf_action: std::sync::Arc::from(""),
            provider_id: 0,
            browser_version: std::sync::Arc::from(""),
            os_version: std::sync::Arc::from(""),
            request_attrs: std::sync::Arc::new(request_attrs),
        }
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
    fn metric_stat_upload_groups_request_count_by_resolved_ip() {
        let aggregator = crate::metrics::aggregator::MetricAggregator::with_request_samples(true);
        for request_id in ["a", "b"] {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("remoteAddr".to_string(), "203.0.113.9".to_string());
            attrs.insert("requestId".to_string(), request_id.to_string());
            attrs.insert(
                "requestURI".to_string(),
                format!("/index.html?rid={request_id}"),
            );
            aggregator.record(metric_key_with_attrs(attrs), 100, 10, false);
        }

        let samples = aggregator.flush();
        assert_eq!(samples.len(), 1);

        let item = metric_item(10, vec!["${remoteAddr}"], "${countRequest}");
        let mut cache = MetricValuesCache::default();
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples, &mut cache);

        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].count, 2);
        assert_eq!(uploads[0].total, 2.0);
        assert_eq!(uploads[0].metric_stats.len(), 1);
        assert_eq!(uploads[0].metric_stats[0].keys, vec!["203.0.113.9"]);
        assert_eq!(uploads[0].metric_stats[0].value, 2.0);
    }

    #[test]
    fn metric_stat_upload_groups_paths_and_sums_bytes() {
        let aggregator = crate::metrics::aggregator::MetricAggregator::with_request_samples(true);
        for (request_id, bytes_sent) in [("a", 123), ("b", 456)] {
            let mut attrs = std::collections::BTreeMap::new();
            attrs.insert("remoteAddr".to_string(), "203.0.113.9".to_string());
            attrs.insert("requestId".to_string(), request_id.to_string());
            attrs.insert(
                "requestURI".to_string(),
                format!("/assets/app.js?rid={request_id}"),
            );
            attrs.insert("requestPath".to_string(), "/assets/app.js".to_string());
            aggregator.record(metric_key_with_attrs(attrs), bytes_sent, 10, false);
        }

        let samples = aggregator.flush();
        let item = metric_item(11, vec!["${requestPath}"], "${countTrafficOut}");
        let mut cache = MetricValuesCache::default();
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples, &mut cache);

        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].count, 2);
        assert_eq!(uploads[0].total, 579.0);
        assert_eq!(uploads[0].metric_stats.len(), 1);
        assert_eq!(uploads[0].metric_stats[0].keys, vec!["/assets/app.js"]);
        assert_eq!(uploads[0].metric_stats[0].value, 579.0);
    }

    #[test]
    fn metric_stat_upload_resolves_method_aliases() {
        let aggregator = crate::metrics::aggregator::MetricAggregator::with_request_samples(true);
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("requestMethod".to_string(), "POST".to_string());
        attrs.insert("requestPath".to_string(), "/submit".to_string());
        attrs.insert("requestURI".to_string(), "/submit?x=1".to_string());
        aggregator.record(metric_key_with_attrs(attrs), 10, 100, false);

        let samples = aggregator.flush();
        let item = metric_item(
            12,
            vec!["${requestMethod}", "${method}", "${request.method}"],
            "${countRequest}",
        );
        let mut cache = MetricValuesCache::default();
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples, &mut cache);

        assert_eq!(uploads.len(), 1);
        assert_eq!(
            uploads[0].metric_stats[0].keys,
            vec!["POST", "POST", "POST"]
        );
        assert_eq!(uploads[0].metric_stats[0].value, 1.0);
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
    fn metric_stat_category_filter_excludes_tcp_rows_from_http_items() {
        fn sample(
            category: &str,
        ) -> (
            crate::metrics::aggregator::AggregationKey,
            crate::metrics::aggregator::AggregatedValue,
        ) {
            (
                crate::metrics::aggregator::AggregationKey {
                    category: std::sync::Arc::from(category),
                    server_id: 1,
                    country: std::sync::Arc::from(""),
                    country_id: 0,
                    province: std::sync::Arc::from(""),
                    province_id: 0,
                    city: std::sync::Arc::from(""),
                    city_id: 0,
                    provider: std::sync::Arc::from("Unknown"),
                    browser: std::sync::Arc::from(""),
                    os: std::sync::Arc::from(""),
                    waf_group_id: 0,
                    waf_action: std::sync::Arc::from(""),
                    provider_id: 0,
                    browser_version: std::sync::Arc::from(""),
                    os_version: std::sync::Arc::from(""),
                    request_attrs: std::sync::Arc::new(std::collections::BTreeMap::new()),
                },
                crate::metrics::aggregator::AggregatedValue {
                    count: 1,
                    ..Default::default()
                },
            )
        }

        let samples = [
            sample(crate::metrics::METRIC_CATEGORY_HTTP),
            sample(crate::metrics::METRIC_CATEGORY_TCP),
        ];
        let http_category = crate::metrics::METRIC_CATEGORY_HTTP.to_string();

        let matching = samples
            .iter()
            .filter(|sample| sample_matches_metric_category(sample, &http_category))
            .count();

        assert_eq!(matching, 1);
        assert!(sample_matches_metric_category(&samples[0], &http_category));
        assert!(!sample_matches_metric_category(&samples[1], &http_category));
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
        DateTime::parse_from_rfc3339(&format!("2026-10-24T12:{minute:02}:00+08:00"))
            .expect("fixed test clock")
    }

    fn http_sample(server_id: i64, count: i64) -> Vec<MetricStatSample> {
        let mut key = metric_key_with_attrs(std::collections::BTreeMap::new());
        key.server_id = server_id;
        vec![(
            key,
            crate::metrics::aggregator::AggregatedValue {
                count,
                bytes_sent: count.saturating_mul(10),
                ..Default::default()
            },
        )]
    }

    fn time_key_for(
        item: &crate::config_models::MetricItemConfig,
        now: &DateTime<FixedOffset>,
    ) -> String {
        get_period_time_at(item.period, &item.period_unit, now)
    }

    struct RecordingMetricSink<F> {
        attempts: Arc<parking_lot::Mutex<Vec<pb::UploadMetricStatsRequest>>>,
        succeeds: F,
    }

    impl<F> MetricUploadSink for RecordingMetricSink<F>
    where
        F: FnMut(&pb::UploadMetricStatsRequest) -> bool,
    {
        fn send(
            &mut self,
            request: &pb::UploadMetricStatsRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
            let ok = (self.succeeds)(request);
            self.attempts.lock().push(request.clone());
            std::future::ready(if ok {
                Ok(())
            } else {
                Err(anyhow::anyhow!("spool full"))
            })
        }
    }

    fn recording_sink<F>(
        attempts: Arc<parking_lot::Mutex<Vec<pb::UploadMetricStatsRequest>>>,
        succeeds: F,
    ) -> RecordingMetricSink<F>
    where
        F: FnMut(&pb::UploadMetricStatsRequest) -> bool,
    {
        RecordingMetricSink { attempts, succeeds }
    }

    #[tokio::test]
    async fn metric_values_cache_commits_after_enqueue_and_preserves_keep_keys() {
        let item = metric_item(701, vec!["${country}"], "${countRequest}");
        let items = vec![item];
        let now = test_now(0);
        let samples = http_sample(1, 2);
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = recording_sink(attempts.clone(), |_| true);
        let mut state = MetricStatReporterState::default();

        let first = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(first.enqueued, 1);
        assert_eq!(state.stats().entries, 1);
        let first_hash = {
            let sent = attempts.lock();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].metric_stats.len(), 1);
            assert!(sent[0].keep_keys.is_empty());
            sent[0].metric_stats[0].hash.clone()
        };

        // Same value in the same window: only keep_keys, and the cache already
        // holds the committed value.
        let second = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(second.enqueued, 1);
        let sent = attempts.lock();
        assert_eq!(sent.len(), 2);
        assert!(sent[1].metric_stats.is_empty());
        assert_eq!(sent[1].keep_keys, vec![first_hash]);
    }

    #[test]
    fn metric_values_cache_evicted_entry_resends_full_stats_instead_of_keep_keys() {
        let item = metric_item(702, vec!["${country}"], "${countRequest}");
        let now = test_now(0);
        let time_key = time_key_for(&item, &now);
        let scope = metric_values_scope(std::slice::from_ref(&item), &now);
        let samples = http_sample(1, 2);
        // One entry is enough to evict the previous one on the next commit.
        let mut cache = MetricValuesCache::new(1, 1_000_000);

        let first = build_metric_uploads_for_item(&item, &time_key, &samples, &mut cache);
        assert_eq!(first.len(), 1);
        commit_metric_upload(&mut cache, &scope, &first[0]);
        assert_eq!(cache.len(), 1);

        let deduped = build_metric_uploads_for_item(&item, &time_key, &samples, &mut cache);
        assert!(deduped[0].metric_stats.is_empty());
        assert_eq!(deduped[0].keep_keys.len(), 1);

        assert!(cache.commit(&scope, item.id, item.version, &time_key, "unrelated", 1.0));
        assert!(
            cache
                .lookup(item.id, item.version, &time_key, &deduped[0].keep_keys[0])
                .is_none()
        );
        let resent = build_metric_uploads_for_item(&item, &time_key, &samples, &mut cache);
        assert_eq!(resent[0].metric_stats.len(), 1);
        assert!(resent[0].keep_keys.is_empty());
    }

    #[test]
    fn metric_values_cache_is_bounded_by_entries_and_estimated_bytes() {
        let mut entries_capped = MetricValuesCache::new(8, 64 * 1024 * 1024);
        let scope = MetricScope::from_windows([(1, 1, "w")]);
        for index in 0..100 {
            assert!(entries_capped.commit(&scope, 1, 1, "w", &format!("h{index}"), index as f32));
        }
        assert!(entries_capped.len() <= 8);
        assert_eq!(entries_capped.stats().evicted_entries, 92);
        assert!(entries_capped.lookup(1, 1, "w", "h0").is_none());
        assert!(entries_capped.lookup(1, 1, "w", "h99").is_some());

        let mut bytes_capped = MetricValuesCache::new(1_000, 256);
        for index in 0..100 {
            bytes_capped.commit(&scope, 1, 1, "w", &format!("h{index}"), index as f32);
        }
        let stats = bytes_capped.stats();
        assert!(stats.estimated_bytes <= 256);
        assert!(stats.entries < 100);
        assert!(stats.evicted_entries > 0);
    }

    #[test]
    fn metric_values_cache_keeps_thousands_of_windows_bounded_and_prunes_them() {
        let mut cache = MetricValuesCache::new(10_000, 64 * 1024 * 1024);
        for window in 0..5_000 {
            let time_key = format!("w{window:05}");
            let scope = MetricScope::from_windows([(1, 1, time_key.clone())]);
            assert!(cache.commit(
                &scope,
                1,
                1,
                &time_key,
                &format!("h{window}"),
                window as f32
            ));
            assert!(cache.len() <= 10_000);
        }
        assert_eq!(cache.len(), 5_000);

        let live = MetricScope::from_windows([(1, 1, "w04999")]);
        assert_eq!(cache.prune(&live), 4_999);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats().estimated_bytes, cache.estimated_bytes());
        assert!(cache.lookup(1, 1, "w04999", "h4999").is_some());
    }

    #[test]
    fn metric_values_cache_prune_drops_disabled_deleted_and_version_bumped_items() {
        let window = "w";
        let mut cache = MetricValuesCache::default();
        let commit_all =
            MetricScope::from_windows([(1, 1, window), (2, 1, window), (3, 1, window)]);
        assert!(cache.commit(&commit_all, 1, 1, window, "h1", 1.0));
        assert!(cache.commit(&commit_all, 2, 1, window, "h2", 2.0));
        assert!(cache.commit(&commit_all, 3, 1, window, "h3", 3.0));
        // A version bump is a different configuration generation: seed a v2
        // entry as well.
        let commit_v2 = MetricScope::from_windows([(1, 2, window)]);
        assert!(cache.commit(&commit_v2, 1, 2, window, "h1-v2", 4.0));

        // Item 2 disabled, item 3 deleted, item 1 bumped to v2.
        let live = MetricScope::from_windows([(1, 2, window)]);
        assert_eq!(cache.prune(&live), 3);
        assert_eq!(cache.len(), 1);
        assert!(cache.lookup(1, 2, window, "h1-v2").is_some());
        assert!(cache.lookup(2, 1, window, "h2").is_none());
        assert!(cache.lookup(3, 1, window, "h3").is_none());
    }

    #[test]
    fn metric_values_cache_clock_rollback_forgets_later_window() {
        let mut cache = MetricValuesCache::default();
        let later = MetricScope::from_windows([(1, 1, "202610241205")]);
        assert!(cache.commit(&later, 1, 1, "202610241205", "h", 7.0));

        let earlier = MetricScope::from_windows([(1, 1, "202610241200")]);
        assert_eq!(cache.prune(&earlier), 1);
        assert_eq!(cache.len(), 0);
        assert!(cache.lookup(1, 1, "202610241200", "h").is_none());
    }

    #[tokio::test]
    async fn metric_stat_prune_runs_without_samples_or_items() {
        let item = metric_item(711, vec!["${country}"], "${countRequest}");
        let now = test_now(0);
        let scope = metric_values_scope(std::slice::from_ref(&item), &now);
        let window = time_key_for(&item, &now);
        let mut state = MetricStatReporterState::default();
        for index in 0..5 {
            assert!(state.cache_mut().commit(
                &scope,
                item.id,
                item.version,
                &window,
                &format!("h{index}"),
                index as f32
            ));
        }
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = recording_sink(attempts.clone(), |_| true);

        // No samples in the same window: the cache is still current.
        let same_window =
            run_metric_stat_tick(&mut state, &[item.clone()], || None, &now, &mut sink).await;
        assert_eq!(same_window.pruned, 0);
        assert_eq!(state.stats().entries, 5);

        // No samples after the window moves: historical entries still go away.
        let next_window =
            run_metric_stat_tick(&mut state, &[item], || None, &test_now(1), &mut sink).await;
        assert_eq!(next_window.pruned, 5);
        assert_eq!(state.stats().entries, 0);
        assert_eq!(state.stats().estimated_bytes, 0);
        assert!(attempts.lock().is_empty());
    }

    #[tokio::test]
    async fn metric_stat_prune_runs_when_item_list_is_empty() {
        let now = test_now(0);
        let scope = MetricScope::from_windows([(1, 1, "w")]);
        let mut state = MetricStatReporterState::default();
        assert!(state.cache_mut().commit(&scope, 1, 1, "w", "h", 1.0));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = recording_sink(attempts.clone(), |_| true);

        let outcome = run_metric_stat_tick(&mut state, &[], || None, &now, &mut sink).await;
        assert_eq!(outcome.pruned, 1);
        assert_eq!(state.stats().entries, 0);
        assert!(attempts.lock().is_empty());
    }

    #[tokio::test]
    async fn metric_stat_disabled_item_clears_cache_and_sends_nothing() {
        let mut item = metric_item(721, vec!["${country}"], "${countRequest}");
        let now = test_now(0);
        let scope = metric_values_scope(std::slice::from_ref(&item), &now);
        let window = time_key_for(&item, &now);
        let samples = http_sample(1, 2);
        let mut state = MetricStatReporterState::default();
        assert!(
            state
                .cache_mut()
                .commit(&scope, item.id, item.version, &window, "h", 2.0)
        );
        item.is_on = false;

        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = recording_sink(attempts.clone(), |_| true);
        let outcome = run_metric_stat_tick(
            &mut state,
            &[item],
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;

        assert_eq!(outcome.pruned, 1);
        assert_eq!(outcome.enqueued, 0);
        assert_eq!(state.stats().entries, 0);
        assert!(attempts.lock().is_empty());
    }

    #[tokio::test]
    async fn metric_stat_spool_failure_retries_exact_payload_without_committing() {
        let items = vec![metric_item(731, vec!["${country}"], "${countRequest}")];
        let now = test_now(0);
        let samples = http_sample(1, 2);
        let healthy = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let healthy_flag = healthy.clone();
        let mut sink = recording_sink(attempts.clone(), move |_| {
            healthy_flag.load(Ordering::Relaxed)
        });
        let mut state = MetricStatReporterState::default();

        let failed = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(failed.enqueued, 0);
        assert_eq!(failed.deferred, 1);
        assert_eq!(state.pending_len(), 1);
        assert_eq!(
            state.stats().entries,
            0,
            "a failed enqueue must not seed the dedup cache"
        );
        let original = {
            let sent = attempts.lock();
            assert_eq!(sent.len(), 1);
            prost::Message::encode_to_vec(&sent[0])
        };

        healthy.store(true, Ordering::Relaxed);
        let retried = run_metric_stat_tick(&mut state, &items, || None, &now, &mut sink).await;
        assert_eq!(retried.enqueued, 1);
        assert_eq!(state.pending_len(), 0);
        assert_eq!(state.stats().entries, 1);

        let sent = attempts.lock();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            prost::Message::encode_to_vec(&sent[1]),
            original,
            "retry must carry the exact retained payload"
        );
    }

    #[tokio::test]
    async fn metric_stat_retained_payload_preserves_item_order_without_blocking_flush() {
        let items = vec![metric_item(751, vec!["${country}"], "${countRequest}")];
        let now = test_now(0);
        let healthy = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let healthy_flag = healthy.clone();
        let mut sink = recording_sink(attempts.clone(), move |_| {
            healthy_flag.load(Ordering::Relaxed)
        });
        let mut state = MetricStatReporterState::default();

        let first = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(http_sample(1, 2)),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(first.deferred, 1);
        assert_eq!(state.pending_len(), 1);

        // The spool is still down, but this tick still flushes. The newer
        // request is appended behind the retained one: it neither overtakes it
        // nor is it dropped.
        let flushed = Arc::new(AtomicBool::new(false));
        let flushed_flag = flushed.clone();
        let blocked = run_metric_stat_tick(
            &mut state,
            &items,
            || {
                flushed_flag.store(true, Ordering::Relaxed);
                Some(http_sample(1, 3))
            },
            &now,
            &mut sink,
        )
        .await;
        assert!(flushed.load(Ordering::Relaxed));
        assert_eq!(blocked.enqueued, 0);
        assert_eq!(state.pending_len(), 2);

        // Recovery delivers the retained payloads in order, then dedups the
        // value that is already durable.
        healthy.store(true, Ordering::Relaxed);
        let recovered = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(http_sample(1, 3)),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(recovered.enqueued, 3);
        assert_eq!(state.pending_len(), 0);

        let sent = attempts.lock();
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[2].metric_stats[0].value, 2.0);
        assert_eq!(sent[3].metric_stats[0].value, 3.0);
        assert_eq!(sent[4].keep_keys.len(), 1);
    }

    #[tokio::test]
    async fn metric_stat_partial_success_does_not_reupload_successful_item() {
        let items = vec![
            metric_item(741, vec!["${country}"], "${countRequest}"),
            metric_item(742, vec!["${country}"], "${countRequest}"),
        ];
        let now = test_now(0);
        let samples = http_sample(1, 2);
        let fail_second = Arc::new(AtomicBool::new(true));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let fail_flag = fail_second.clone();
        let mut sink = recording_sink(attempts.clone(), move |request| {
            !(fail_flag.load(Ordering::Relaxed) && request.item_id == 742)
        });
        let mut state = MetricStatReporterState::default();

        let partial = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(partial.enqueued, 1);
        assert_eq!(partial.deferred, 1);
        assert_eq!(state.pending_len(), 1);
        assert_eq!(state.stats().entries, 1);
        fail_second.store(false, Ordering::Relaxed);
        {
            let sent = attempts.lock();
            assert_eq!(
                sent.iter().map(|r| r.item_id).collect::<Vec<_>>(),
                vec![741, 742]
            );
            assert!(!sent[0].metric_stats.is_empty());
        }

        let retried = run_metric_stat_tick(&mut state, &items, || None, &now, &mut sink).await;
        assert_eq!(retried.enqueued, 1);
        assert_eq!(state.pending_len(), 0);
        assert_eq!(state.stats().entries, 2);

        let same_batch = run_metric_stat_tick(
            &mut state,
            &items,
            || Some(samples.clone()),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(same_batch.enqueued, 2);

        let sent = attempts.lock();
        let successful: Vec<_> = sent.iter().filter(|r| r.item_id == 741).collect();
        assert_eq!(successful.len(), 2);
        assert_eq!(
            successful
                .iter()
                .filter(|r| !r.metric_stats.is_empty())
                .count(),
            1,
            "the succeeded item must not be re-uploaded as a new data row"
        );
        assert!(
            successful[1].metric_stats.is_empty(),
            "the next batch may only dedup the unchanged value"
        );
        let failed: Vec<_> = sent.iter().filter(|r| r.item_id == 742).collect();
        assert_eq!(failed.len(), 3);
        assert!(failed[2].metric_stats.is_empty());
    }

    const MIB: usize = 1024 * 1024;

    #[test]
    fn metric_stat_request_over_wire_budget_is_split_without_losing_records() {
        let budget = metric_stat_payload_budget();
        assert!(
            budget < 6 * MIB,
            "the envelope must leave headroom below 6 MiB"
        );
        assert!(budget > 5 * MIB, "the budget should not collapse");

        let rows: Vec<_> = (0..60_000)
            .map(|index| pb::UploadingMetricStat {
                id: index,
                hash: format!("h{index:05}{}", "x".repeat(120)),
                keys: Vec::new(),
                value: index as f32,
            })
            .collect();
        let request = pb::UploadMetricStatsRequest {
            server_id: 9,
            time: "202610241200".into(),
            count: 7,
            total: 1.5,
            version: 3,
            item_id: 55,
            metric_stats: rows,
            keep_keys: Vec::new(),
        };
        assert!(prost::Message::encoded_len(&request) > 7 * MIB);

        let split = split_metric_stat_request(request, budget);
        assert!(split.quarantine.is_empty());
        assert!(
            split.chunks.len() >= 2,
            "a 7 MiB payload needs several chunks"
        );

        let mut rows = 0;
        for chunk in &split.chunks {
            assert!(
                prost::Message::encoded_len(chunk) <= budget,
                "every chunk must fit the encoded wire budget"
            );
            assert_eq!(chunk.server_id, 9);
            assert_eq!(chunk.item_id, 55);
            assert_eq!(chunk.version, 3);
            assert_eq!(chunk.time, "202610241200");
            // Request-level aggregates are copied to every chunk so each row
            // keeps exactly the values it carried in the unsplit request.
            assert_eq!(chunk.count, 7);
            assert_eq!(chunk.total, 1.5);
            rows += chunk.metric_stats.len();
        }
        assert_eq!(rows, 60_000, "splitting must not drop a row");
    }

    #[test]
    fn metric_stat_split_quarantines_only_the_oversized_record() {
        let budget = metric_stat_payload_budget();
        let giant = pb::UploadingMetricStat {
            id: 1,
            hash: "g".repeat(budget + 1_024),
            keys: vec!["k".repeat(budget)],
            value: 1.0,
        };
        let normal = pb::UploadingMetricStat {
            id: 2,
            hash: "ok".into(),
            keys: Vec::new(),
            value: 2.0,
        };
        let request = pb::UploadMetricStatsRequest {
            server_id: 11,
            time: "w".into(),
            count: 1,
            total: 2.0,
            version: 1,
            item_id: 66,
            metric_stats: vec![giant, normal],
            keep_keys: vec!["k".repeat(budget + 1)],
        };

        let split = split_metric_stat_request(request, budget);
        assert_eq!(split.chunks.len(), 1);
        assert_eq!(split.chunks[0].metric_stats.len(), 1);
        assert_eq!(split.chunks[0].metric_stats[0].hash, "ok");
        assert!(split.chunks[0].keep_keys.is_empty());
        // The oversized row and keep key keep their complete payload, each in
        // its own single-record request, for the durable quarantine.
        assert_eq!(split.quarantine.len(), 2);
        assert_eq!(split.quarantine[0].metric_stats.len(), 1);
        assert_eq!(split.quarantine[0].metric_stats[0].id, 1);
        assert_eq!(split.quarantine[0].metric_stats[0].keys.len(), 1);
        assert!(split.quarantine[0].keep_keys.is_empty());
        assert!(split.quarantine[1].metric_stats.is_empty());
        assert_eq!(split.quarantine[1].keep_keys.len(), 1);
    }

    /// Fails identically on every send. `refusal` picks the error class the
    /// retry policy is supposed to distinguish.
    struct RefusingMetricSink {
        attempts: Arc<parking_lot::Mutex<Vec<pb::UploadMetricStatsRequest>>>,
        refusal: Refusal,
    }

    #[derive(Clone, Copy)]
    enum Refusal {
        Capacity,
        BatchTooLarge,
    }

    impl MetricUploadSink for RefusingMetricSink {
        fn send(
            &mut self,
            request: &pb::UploadMetricStatsRequest,
        ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send {
            self.attempts.lock().push(request.clone());
            let error = match self.refusal {
                Refusal::Capacity => anyhow::Error::new(crate::telemetry::SpoolError::Capacity {
                    required: 1,
                    available: 0,
                    core: true,
                }),
                Refusal::BatchTooLarge => {
                    anyhow::Error::new(crate::telemetry::SpoolError::BatchTooLarge {
                        bytes: 7 * MIB,
                        limit: 6 * MIB,
                    })
                }
            };
            std::future::ready(Err(error))
        }
    }

    fn temp_quarantine_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "zelvik-metric-quarantine-test-{label}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn read_quarantine(dir: &std::path::Path) -> Vec<pb::UploadMetricStatsRequest> {
        let Ok(entries) = std::fs::read_dir(dir.join("quarantine")) else {
            return Vec::new();
        };
        let mut requests = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("pb") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read quarantined payload");
            let request =
                <pb::UploadMetricStatsRequest as prost::Message>::decode(bytes.as_slice())
                    .expect("decode quarantined payload");
            requests.push(request);
        }
        requests
    }

    fn seeded_request(item_id: i64, value: f32) -> pb::UploadMetricStatsRequest {
        pb::UploadMetricStatsRequest {
            server_id: 1,
            time: "w".into(),
            count: 1,
            total: value,
            version: 1,
            item_id,
            metric_stats: vec![pb::UploadingMetricStat {
                id: item_id,
                hash: "h".into(),
                keys: vec!["dim".into()],
                value,
            }],
            keep_keys: Vec::new(),
        }
    }

    fn seed_spool_pending(state: &mut MetricStatReporterState, item_id: i64) {
        state.push_pending(
            item_id,
            PendingMetricUpload::for_spool(seeded_request(item_id, 1.0)),
        );
    }

    fn seed_quarantine_pending(state: &mut MetricStatReporterState, item_id: i64) {
        state.push_pending(
            item_id,
            PendingMetricUpload::for_quarantine(seeded_request(item_id, 1.0)),
        );
    }

    #[tokio::test]
    async fn metric_stat_capacity_refusal_is_retried_without_bound_or_loss() {
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = RefusingMetricSink {
            attempts: attempts.clone(),
            refusal: Refusal::Capacity,
        };
        let mut state = MetricStatReporterState::default();
        seed_spool_pending(&mut state, 771);

        for _ in 0..20 {
            let outcome =
                run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
            assert_eq!(outcome.enqueued, 0);
            assert_eq!(state.pending_len(), 1);
            assert_eq!(state.pending_bytes(), state.stats().pending_bytes);
        }
        assert_eq!(state.stats().terminal_failed_uploads, 0);
        assert_eq!(state.stats().terminal_oversize_records, 0);
        assert_eq!(
            attempts.lock().len(),
            20,
            "capacity keeps the exact payload"
        );
    }

    #[tokio::test]
    async fn metric_stat_wire_refusal_moves_full_payload_to_durable_quarantine() {
        let dir = temp_quarantine_dir("wire-refusal");
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = RefusingMetricSink {
            attempts: attempts.clone(),
            refusal: Refusal::BatchTooLarge,
        };
        let mut state = MetricStatReporterState::with_quarantine_dir(dir.clone());
        seed_spool_pending(&mut state, 772);

        // The spool refuses the bytes: the exact request is reclassified and
        // stays pending, not dropped.
        run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
        assert_eq!(state.pending_len(), 1);
        assert_eq!(state.stats().terminal_oversize_records, 0);

        // The next tick stores the complete request durably.
        let stored = run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
        assert_eq!(stored.enqueued, 0);
        assert_eq!(state.pending_len(), 0);
        assert_eq!(state.stats().terminal_oversize_records, 1);
        assert!(state.stats().quarantined_bytes > 0);
        let quarantined = read_quarantine(&dir);
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0], seeded_request(772, 1.0));

        run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
        assert_eq!(
            attempts.lock().len(),
            1,
            "it is never offered to the wire again"
        );
        assert_eq!(read_quarantine(&dir).len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn metric_stat_quarantine_store_failure_retains_payload_until_it_fits() {
        let dir = temp_quarantine_dir("quota");
        let mut state = MetricStatReporterState::with_quarantine_dir(dir.clone());
        state.set_quarantine_max_bytes(1);
        seed_quarantine_pending(&mut state, 773);
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut sink = RefusingMetricSink {
            attempts,
            refusal: Refusal::Capacity,
        };

        let refused = run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
        assert_eq!(refused.enqueued, 0);
        assert_eq!(
            state.pending_len(),
            1,
            "a full quarantine never drops the payload"
        );
        assert_eq!(state.stats().terminal_oversize_records, 0);
        assert!(read_quarantine(&dir).is_empty());

        state.set_quarantine_max_bytes(1 << 20);
        run_metric_stat_tick(&mut state, &[], || None, &test_now(0), &mut sink).await;
        assert_eq!(state.pending_len(), 0);
        assert_eq!(state.stats().terminal_oversize_records, 1);
        let quarantined = read_quarantine(&dir);
        assert_eq!(quarantined.len(), 1);
        assert_eq!(quarantined[0], seeded_request(773, 1.0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn metric_stat_pending_payloads_survive_restart() {
        let dir = temp_quarantine_dir("restart");
        let now = test_now(0);
        let item = metric_item(795, vec!["${country}"], "${countRequest}");
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut failing = recording_sink(attempts.clone(), |_| false);
        let mut first = MetricStatReporterState::with_quarantine_dir(dir.clone());

        let first_tick = run_metric_stat_tick(
            &mut first,
            &[item],
            || Some(http_sample(1, 2)),
            &now,
            &mut failing,
        )
        .await;
        assert_eq!(first_tick.deferred, 1);
        assert_eq!(first.pending_len(), 1);
        assert_eq!(
            std::fs::read_dir(dir.join("pending"))
                .expect("pending dir")
                .count(),
            1,
            "a payload that left the aggregator must be durable before restart"
        );
        drop(first);

        // A fresh process recovers the exact request from the pending dir.
        let mut recovered = MetricStatReporterState::with_quarantine_dir(dir.clone());
        assert_eq!(recovered.recover_pending(), 1);
        assert_eq!(recovered.pending_len(), 1);
        let mut healthy = recording_sink(attempts.clone(), |_| true);
        let resumed = run_metric_stat_tick(&mut recovered, &[], || None, &now, &mut healthy).await;
        assert_eq!(resumed.enqueued, 1);
        assert_eq!(recovered.pending_len(), 0);
        assert_eq!(
            std::fs::read_dir(dir.join("pending"))
                .expect("pending dir")
                .count(),
            0,
            "the durable copy is removed only after the spool accepted it"
        );
        let sent = attempts.lock();
        let recovered_request = sent.last().expect("recovered send");
        assert_eq!(recovered_request.item_id, 795);
        assert_eq!(recovered_request.metric_stats[0].value, 2.0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn metric_stat_permanent_failure_blocks_only_its_item_and_never_drops() {
        let failing = metric_item(781, vec!["${country}"], "${countRequest}");
        let healthy = metric_item(782, vec!["${country}"], "${countRequest}");
        let now = test_now(0);
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let fail_failing = Arc::new(AtomicBool::new(true));
        let flag = fail_failing.clone();
        let mut sink = recording_sink(attempts.clone(), move |request| {
            !(flag.load(Ordering::Relaxed) && request.item_id == 781)
        });
        let mut state = MetricStatReporterState::default();

        let first = run_metric_stat_tick(
            &mut state,
            &[failing.clone(), healthy.clone()],
            || Some(http_sample(1, 2)),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(first.enqueued, 1, "the healthy item must not be blocked");
        assert_eq!(first.deferred, 1);
        assert_eq!(state.pending_len(), 1);
        assert_eq!(state.stats().entries, 1);

        // The failing item is permanently refused. It keeps every payload, and
        // the healthy item keeps uploading on every tick; the failing queue is
        // partitioned and never gates the healthy one.
        for offset in 0..5i64 {
            let outcome = run_metric_stat_tick(
                &mut state,
                &[failing.clone(), healthy.clone()],
                || Some(http_sample(1, 3 + offset)),
                &now,
                &mut sink,
            )
            .await;
            assert_eq!(outcome.enqueued, 1, "the healthy item still uploads");
            assert_eq!(state.pending_len() as i64, 2 + offset);
            assert_eq!(state.stats().entries, 1);
        }
        let queued = state.pending_len();
        assert_eq!(queued, 6);

        // Disabling/deleting the failing item must not turn its retained data
        // into a drop, and must not poison the healthy item.
        for _ in 0..3 {
            let outcome = run_metric_stat_tick(
                &mut state,
                &[healthy.clone()],
                || Some(http_sample(1, 2)),
                &now,
                &mut sink,
            )
            .await;
            assert_eq!(outcome.enqueued, 1);
            assert_eq!(state.pending_len(), queued, "no queued payload is dropped");
        }
        assert!(state.stats().pending_bytes > 0);
        assert_eq!(state.stats().backpressured, false);

        // Recovery delivers every retained payload, oldest first.
        fail_failing.store(false, Ordering::Relaxed);
        let recovered = run_metric_stat_tick(
            &mut state,
            &[healthy.clone()],
            || Some(http_sample(1, 2)),
            &now,
            &mut sink,
        )
        .await;
        assert_eq!(recovered.enqueued, queued + 1);
        assert_eq!(state.pending_len(), 0);
        assert_eq!(state.pending_bytes(), 0);

        let sent = attempts.lock();
        let failing_rows: Vec<_> = sent.iter().filter(|r| r.item_id == 781).collect();
        let delivered: Vec<_> = failing_rows
            .iter()
            .filter(|r| !r.metric_stats.is_empty())
            .map(|r| r.metric_stats[0].value)
            .collect();
        assert_eq!(
            &delivered[delivered.len() - 6..],
            &[2.0, 3.0, 4.0, 5.0, 6.0, 7.0],
            "the recovery tick must deliver every retained payload in order"
        );
    }

    #[tokio::test]
    async fn metric_stat_backpressure_retains_samples_in_aggregator_without_dropping() {
        let dir = temp_quarantine_dir("backpressure");
        let healthy = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let flag = healthy.clone();
        let mut sink = recording_sink(attempts.clone(), move |_| flag.load(Ordering::Relaxed));
        let mut state = MetricStatReporterState::with_quarantine(100, 1 << 20, 1, dir.clone());
        seed_spool_pending(&mut state, 791);

        let flushed = Arc::new(AtomicBool::new(false));
        let flushed_flag = flushed.clone();
        let first = run_metric_stat_tick(
            &mut state,
            &[metric_item(792, vec!["${country}"], "${countRequest}")],
            || {
                flushed_flag.store(true, Ordering::Relaxed);
                Some(http_sample(1, 2))
            },
            &test_now(0),
            &mut sink,
        )
        .await;
        assert_eq!(first.enqueued, 0);
        assert!(
            !flushed.load(Ordering::Relaxed),
            "backpressure must not flush"
        );
        assert_eq!(state.pending_len(), 1);
        assert!(state.stats().backpressured);
        assert!(state.stats().pending_bytes > 1);

        // The spool recovers: the retained payload drains, then the samples
        // still held in the aggregator are flushed normally.
        healthy.store(true, Ordering::Relaxed);
        let resumed = run_metric_stat_tick(
            &mut state,
            &[metric_item(792, vec!["${country}"], "${countRequest}")],
            || {
                flushed_flag.store(true, Ordering::Relaxed);
                Some(http_sample(1, 2))
            },
            &test_now(0),
            &mut sink,
        )
        .await;
        assert_eq!(resumed.enqueued, 2);
        assert!(flushed.load(Ordering::Relaxed));
        assert!(!state.stats().backpressured);
        assert_eq!(state.pending_len(), 0);

        std::fs::remove_dir_all(&dir).ok();
    }
}
