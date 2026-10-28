use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::metrics::ServerStatusSnapshot;
use crate::pb;
use crate::rpc::client::SharedRpcClient;
use chrono::{Datelike, Duration as ChronoDuration, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
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

fn get_period_time(period: i32, unit: &str) -> String {
    let now = crate::utils::time::now_local();
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
            keep_keys: Vec::new(),
        });
    }

    requests
}

const METRIC_PENDING_MAX_REQUESTS: usize = 4_096;
const METRIC_PENDING_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Metric uploads that failed to persist, retained in memory until the spool
/// accepts them. The queue is bounded: metric rows are analytics and may be
/// dropped under sustained local storage failure, but every drop is counted and
/// reported through the telemetry gap path.
#[derive(Default)]
struct MetricPendingUploads {
    requests: Vec<pb::UploadMetricStatsRequest>,
    bytes: u64,
    dropped_rows: u64,
}

impl MetricPendingUploads {
    fn estimated_bytes(request: &pb::UploadMetricStatsRequest) -> u64 {
        prost::Message::encoded_len(request) as u64
            + std::mem::size_of::<pb::UploadMetricStatsRequest>() as u64
    }

    fn push(&mut self, request: pb::UploadMetricStatsRequest) {
        let bytes = Self::estimated_bytes(&request);
        if self.requests.len() >= METRIC_PENDING_MAX_REQUESTS
            || self.bytes.saturating_add(bytes) > METRIC_PENDING_MAX_BYTES
        {
            self.dropped_rows += request.metric_stats.len().max(1) as u64;
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.requests.push(request);
    }

    fn record_lost(&mut self, rows: usize) {
        self.dropped_rows += rows as u64;
    }

    fn drain(&mut self) -> std::vec::IntoIter<pb::UploadMetricStatsRequest> {
        self.bytes = 0;
        std::mem::take(&mut self.requests).into_iter()
    }

    fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
}

pub async fn start_metric_stat_reporter(config_store: Arc<crate::config::ConfigStore>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    let mut pending = MetricPendingUploads::default();
    let mut reported_drops = 0u64;

    loop {
        interval.tick().await;
        if pending.dropped_rows > reported_drops {
            crate::telemetry::record_gap(
                "metric_stat",
                crate::utils::time::now_timestamp(),
                "metric_pending_capacity",
            );
            reported_drops = pending.dropped_rows;
        }

        // Retry older requests first. A failure stops the retry loop and keeps
        // the failed request plus everything behind it in the bounded queue;
        // nothing that already reached the spool is replayed.
        let mut retry_failed = false;
        for request in pending.drain() {
            if retry_failed {
                pending.push(request);
                continue;
            }
            match crate::telemetry::enqueue_metric_stat(request).await {
                Ok(()) => {}
                Err(error) => {
                    let source = error.to_string();
                    let lost = error.lost();
                    let unsent = error.into_unsent();
                    warn!(
                        server_id = unsent.server_id,
                        item_id = unsent.item_id,
                        unsent_rows = unsent.metric_stats.len(),
                        lost,
                        error = %source,
                        "failed to persist pending metric stats; retained"
                    );
                    if lost > 0 {
                        pending.record_lost(lost);
                    }
                    if !unsent.metric_stats.is_empty() {
                        pending.push(unsent);
                    }
                    retry_failed = true;
                }
            }
        }

        let metric_items = config_store.get_metric_items().await;
        if metric_items.is_empty() {
            continue;
        }

        let samples = crate::metrics::aggregator::METRIC_STAT_AGGREGATOR.flush();
        if samples.is_empty() {
            continue;
        }

        let mut uploads = Vec::new();
        for item in metric_items {
            if !item.is_on {
                continue;
            }
            let item_category = metric_item_category(&item);
            let time_key = get_period_time(item.period, &item.period_unit);
            if !samples
                .iter()
                .any(|sample| sample_matches_metric_category(sample, &item_category))
            {
                continue;
            }
            uploads.extend(build_metric_uploads_for_item(&item, &time_key, &samples));
        }

        if !pending.is_empty() {
            for request in uploads {
                pending.push(request);
            }
            continue;
        }
        let mut uploads = uploads.into_iter();
        while let Some(request) = uploads.next() {
            match crate::telemetry::enqueue_metric_stat(request).await {
                Ok(()) => {}
                Err(error) => {
                    let source = error.to_string();
                    let lost = error.lost();
                    let unsent = error.into_unsent();
                    warn!(
                        server_id = unsent.server_id,
                        item_id = unsent.item_id,
                        unsent_rows = unsent.metric_stats.len(),
                        lost,
                        error = %source,
                        "failed to persist metric stats; retained"
                    );
                    if lost > 0 {
                        pending.record_lost(lost);
                    }
                    if !unsent.metric_stats.is_empty() {
                        pending.push(unsent);
                    }
                    for remaining in uploads {
                        pending.push(remaining);
                    }
                    break;
                }
            }
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
    fn metric_pending_uploads_are_bounded_and_drain_resets_accounting() {
        let request = |index: usize| pb::UploadMetricStatsRequest {
            server_id: index as i64,
            metric_stats: vec![pb::UploadingMetricStat {
                keys: vec![format!("k_{index}")],
                value: 1.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut pending = MetricPendingUploads::default();
        for index in 0..METRIC_PENDING_MAX_REQUESTS + 10 {
            pending.push(request(index));
        }
        assert_eq!(pending.requests.len(), METRIC_PENDING_MAX_REQUESTS);
        assert_eq!(pending.dropped_rows, 10);
        assert!(pending.bytes > 0);

        let drained: Vec<_> = pending.drain().collect();
        assert_eq!(drained.len(), METRIC_PENDING_MAX_REQUESTS);
        assert_eq!(drained[0].server_id, 0);
        assert_eq!(
            drained.last().unwrap().server_id as usize,
            METRIC_PENDING_MAX_REQUESTS - 1
        );
        assert_eq!(pending.bytes, 0);
        assert!(pending.is_empty());
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
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples);

        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].count, 2);
        assert_eq!(uploads[0].total, 2.0);
        assert_eq!(uploads[0].metric_stats.len(), 1);
        assert_eq!(uploads[0].metric_stats[0].keys, vec!["203.0.113.9"]);
        assert_eq!(uploads[0].metric_stats[0].value, 2.0);
        assert!(uploads[0].keep_keys.is_empty());
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
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples);

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
        let uploads = build_metric_uploads_for_item(&item, "202606221200", &samples);

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
}
