use chrono::Timelike;
use dashmap::DashMap;
use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock as OnceCell;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use std::sync::atomic::AtomicU32;

static CACHED_PRESSURE: AtomicU32 = AtomicU32::new(0);
static HTTP_DIMENSION_HANDLE: OnceCell<HttpDimensionHandle> = OnceCell::new();
static HTTP_DIMENSION_DROPPED: AtomicU64 = AtomicU64::new(0);
static EMPTY_ARC_STR: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from(""));
static UNKNOWN_ARC_STR: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("Unknown"));
static HTTP_DIMENSION_WARNING_INTERVAL_MS: u64 = 5_000;

pub const METRIC_CATEGORY_HTTP: &str = "http";
pub const METRIC_CATEGORY_TCP: &str = "tcp";
pub const METRIC_CATEGORY_UDP: &str = "udp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShadowTransportKind {
    DownstreamTcp,
    UpstreamTcp,
    RpcTransport,
    UdpSession,
    QuicConnection,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ShadowConnectionMetrics {
    pub downstream_transports: i64,
    pub upstream_transports: i64,
    pub rpc_transports: i64,
    pub udp_sessions: i64,
    pub quic_connections: i64,
    pub pending_requests: i64,
    pub forwarding_requests: i64,
    pub active_requests: i64,
}

struct ShadowConnectionCounters {
    downstream_transports: AtomicI64,
    upstream_transports: AtomicI64,
    rpc_transports: AtomicI64,
    udp_sessions: AtomicI64,
    quic_connections: AtomicI64,
    pending_requests: AtomicI64,
    forwarding_requests: AtomicI64,
    active_requests: AtomicI64,
}

static SHADOW_CONNECTION_COUNTERS: ShadowConnectionCounters = ShadowConnectionCounters {
    downstream_transports: AtomicI64::new(0),
    upstream_transports: AtomicI64::new(0),
    rpc_transports: AtomicI64::new(0),
    udp_sessions: AtomicI64::new(0),
    quic_connections: AtomicI64::new(0),
    pending_requests: AtomicI64::new(0),
    forwarding_requests: AtomicI64::new(0),
    active_requests: AtomicI64::new(0),
};

pub fn shadow_connection_metrics() -> ShadowConnectionMetrics {
    ShadowConnectionMetrics {
        downstream_transports: SHADOW_CONNECTION_COUNTERS
            .downstream_transports
            .load(Ordering::Relaxed),
        upstream_transports: SHADOW_CONNECTION_COUNTERS
            .upstream_transports
            .load(Ordering::Relaxed),
        rpc_transports: SHADOW_CONNECTION_COUNTERS
            .rpc_transports
            .load(Ordering::Relaxed),
        udp_sessions: SHADOW_CONNECTION_COUNTERS
            .udp_sessions
            .load(Ordering::Relaxed),
        quic_connections: SHADOW_CONNECTION_COUNTERS
            .quic_connections
            .load(Ordering::Relaxed),
        pending_requests: SHADOW_CONNECTION_COUNTERS
            .pending_requests
            .load(Ordering::Relaxed),
        forwarding_requests: SHADOW_CONNECTION_COUNTERS
            .forwarding_requests
            .load(Ordering::Relaxed),
        active_requests: SHADOW_CONNECTION_COUNTERS
            .active_requests
            .load(Ordering::Relaxed),
    }
}

pub struct ShadowTransportMetricsGuard {
    counter: &'static AtomicI64,
}

impl Drop for ShadowTransportMetricsGuard {
    fn drop(&mut self) {
        decrement_active_connections(self.counter);
    }
}

pub fn transport_metrics_guard(kind: ShadowTransportKind) -> ShadowTransportMetricsGuard {
    let counter = match kind {
        ShadowTransportKind::DownstreamTcp => &SHADOW_CONNECTION_COUNTERS.downstream_transports,
        ShadowTransportKind::UpstreamTcp => &SHADOW_CONNECTION_COUNTERS.upstream_transports,
        ShadowTransportKind::RpcTransport => &SHADOW_CONNECTION_COUNTERS.rpc_transports,
        ShadowTransportKind::UdpSession => &SHADOW_CONNECTION_COUNTERS.udp_sessions,
        ShadowTransportKind::QuicConnection => &SHADOW_CONNECTION_COUNTERS.quic_connections,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    ShadowTransportMetricsGuard { counter }
}

pub struct ShadowRequestMetricsGuard {
    counter: &'static AtomicI64,
}

impl Drop for ShadowRequestMetricsGuard {
    fn drop(&mut self) {
        decrement_active_connections(self.counter);
    }
}

pub fn pending_request_metrics_guard() -> ShadowRequestMetricsGuard {
    SHADOW_CONNECTION_COUNTERS
        .pending_requests
        .fetch_add(1, Ordering::Relaxed);
    ShadowRequestMetricsGuard {
        counter: &SHADOW_CONNECTION_COUNTERS.pending_requests,
    }
}

pub fn forwarding_metrics_guard() -> ShadowRequestMetricsGuard {
    SHADOW_CONNECTION_COUNTERS
        .forwarding_requests
        .fetch_add(1, Ordering::Relaxed);
    ShadowRequestMetricsGuard {
        counter: &SHADOW_CONNECTION_COUNTERS.forwarding_requests,
    }
}

pub fn active_request_metrics_guard() -> ShadowRequestMetricsGuard {
    SHADOW_CONNECTION_COUNTERS
        .active_requests
        .fetch_add(1, Ordering::Relaxed);
    ShadowRequestMetricsGuard {
        counter: &SHADOW_CONNECTION_COUNTERS.active_requests,
    }
}

pub fn normalize_metric_category(category: &str) -> String {
    let category = category.trim().to_ascii_lowercase();
    if category.is_empty() {
        METRIC_CATEGORY_HTTP.to_string()
    } else {
        category
    }
}

pub fn start_pressure_updater() {
    tokio::spawn(async {
        let mut sys = sysinfo::System::new();
        loop {
            let pressure = compute_node_pressure(&mut sys);
            CACHED_PRESSURE.store(pressure.to_bits(), Ordering::Relaxed);

            // Runtime distinct-IP cardinality is strictly bounded by its
            // governor-derived tracker; daily exact tracking remains separate.

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });
}

fn compute_node_pressure(sys: &mut sysinfo::System) -> f32 {
    let (_, _, total_conns) = METRICS.get_node_totals();
    let cpu_cores = num_cpus::get() as i64;
    let conn_pressure = (total_conns as f64 / (cpu_cores * 2000) as f64).min(1.0);

    sys.refresh_cpu_usage();
    let cpu_load = sys.global_cpu_usage() as f64 / 100.0;

    // Memory was the missing term: an L2 node under governor-classified
    // memory pressure must look pressured to L1 peers and to the local
    // transform gate even when CPU/connections are idle. The floor values
    // map to `limit_for_pressure` steps (0.55→3/4, 0.75→1/2, 0.90→1) so
    // memory pressure can only raise the signal, never lower it.
    let memory_component = match crate::memory_governor::MEMORY_GOVERNOR
        .snapshot(crate::memory_governor::MEMORY_GOVERNOR.pingora_worker_threads())
        .memory_pressure_level
    {
        crate::memory_governor::MemoryPressureLevel::Normal => 0.0_f64,
        crate::memory_governor::MemoryPressureLevel::Elevated => 0.55,
        crate::memory_governor::MemoryPressureLevel::High => 0.75,
        crate::memory_governor::MemoryPressureLevel::Critical => 0.90,
    };

    ((conn_pressure * 0.7 + cpu_load * 0.3)
        .min(1.0)
        .max(memory_component)) as f32
}

pub mod aggregator;
pub mod analyzer;
pub mod daily;
#[cfg(test)]
mod mace_backend;
pub mod storage;
pub mod top_ip;

#[derive(Clone)]
pub struct HttpDimensionEvent {
    pub category: Arc<str>,
    pub server_id: i64,
    pub client_ip: std::net::IpAddr,
    pub domain: Arc<str>,
    pub user_agent: Arc<str>,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub cached_bytes: i64,
    pub waf_group_id: i64,
    pub waf_action: Option<Arc<str>>,
    pub analyzed: Option<crate::metrics::analyzer::RequestStats>,
    pub request_context: Arc<MetricRequestContext>,
    pub created_at: i64,
}

pub struct HttpDimensionsArgs<'a> {
    pub server_id: i64,
    pub client_ip: std::net::IpAddr,
    pub domain: &'a str,
    pub user_agent: &'a str,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub cached_bytes: i64,
    pub waf_group_id: i64,
    pub waf_action: Option<&'a str>,
    pub cached_analyzed: Option<&'a crate::metrics::analyzer::RequestStats>,
    pub request_context: Option<MetricRequestContext>,
}

pub struct NetworkDimensionsArgs<'a> {
    pub category: &'a str,
    pub server_id: i64,
    pub client_ip: std::net::IpAddr,
    pub domain: &'a str,
    pub user_agent: &'a str,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub status: u16,
}

struct DimensionsArgs<'a> {
    category: &'a str,
    server_id: i64,
    client_ip: std::net::IpAddr,
    domain: &'a str,
    user_agent: &'a str,
    bytes_sent: i64,
    bytes_received: i64,
    cached_bytes: i64,
    waf_group_id: i64,
    waf_action: Option<&'a str>,
    cached_analyzed: Option<&'a crate::metrics::analyzer::RequestStats>,
    request_context: Option<MetricRequestContext>,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MetricRequestContext {
    pub values: BTreeMap<String, String>,
}

impl MetricRequestContext {
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert(key.into(), value.into());
    }

    pub fn resolve_key(&self, key: &str) -> String {
        resolve_template_with(key, |var_name| self.resolve_variable(var_name))
    }

    pub fn resolve_variable(&self, var_name: &str) -> String {
        resolve_metric_variable_from_map(&self.values, var_name)
    }

    pub fn network(
        domain: &str,
        client_ip: std::net::IpAddr,
        user_agent: &str,
        bytes_sent: i64,
        bytes_received: i64,
        status: u16,
    ) -> Self {
        let mut values = BTreeMap::new();
        let domain = domain.trim();
        let remote_addr = client_ip.to_string();
        values.insert("host".to_string(), domain.to_string());
        values.insert("serverName".to_string(), domain.to_string());
        values.insert("remoteAddr".to_string(), remote_addr.clone());
        values.insert("remoteAddrValue".to_string(), remote_addr.clone());
        values.insert("rawRemoteAddr".to_string(), remote_addr);
        values.insert("userAgent".to_string(), user_agent.to_string());
        values.insert("status".to_string(), status.to_string());
        values.insert("statusMessage".to_string(), status_message(status));
        values.insert("bytesSent".to_string(), bytes_sent.to_string());
        values.insert("bodyBytesSent".to_string(), bytes_sent.to_string());
        values.insert("requestLength".to_string(), bytes_received.to_string());
        Self { values }
    }
}

pub fn resolve_metric_variable_from_map(
    values: &BTreeMap<String, String>,
    var_name: &str,
) -> String {
    match var_name {
        "requestMethod" | "method" | "request.method" => {
            return metric_method_value(values);
        }
        _ => {}
    }

    if let Some(value) = values.get(var_name) {
        return value.clone();
    }

    match var_name {
        "path" => return values.get("requestPath").cloned().unwrap_or_default(),
        "uri" => {
            return values
                .get("requestURI")
                .or_else(|| values.get("requestUri"))
                .cloned()
                .unwrap_or_default();
        }
        _ => {}
    }

    let Some((prefix, suffix)) = var_name.split_once('.') else {
        return format!("${{{}}}", var_name);
    };

    match prefix {
        "header" | "http" => lookup_case_insensitive(values, &format!("header.{suffix}")),
        "response" => {
            if suffix == "contentType" {
                values
                    .get("response.contentType")
                    .cloned()
                    .or_else(|| {
                        Some(lookup_case_insensitive(
                            values,
                            "response.header.Content-Type",
                        ))
                        .filter(|value| !value.is_empty())
                    })
                    .unwrap_or_default()
            } else if let Some(header) = suffix.strip_prefix("header.") {
                lookup_case_insensitive(values, &format!("response.header.{header}"))
            } else {
                String::new()
            }
        }
        "request" => match suffix {
            "method" => metric_method_value(values),
            "path" => values.get("requestPath").cloned().unwrap_or_default(),
            "uri" | "URI" => values
                .get("requestURI")
                .or_else(|| values.get("requestUri"))
                .cloned()
                .unwrap_or_default(),
            _ => String::new(),
        },
        "cookie" | "arg" | "origin" | "node" | "geo" | "isp" | "browser" | "product" | "host" => {
            values.get(var_name).cloned().unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn metric_method_value(values: &BTreeMap<String, String>) -> String {
    values
        .get("requestMethod")
        .or_else(|| values.get("method"))
        .or_else(|| values.get("request.method"))
        .map(|method| normalize_metric_http_method(method))
        .unwrap_or_default()
}

pub fn normalize_metric_http_method(method: &str) -> String {
    method.trim().to_ascii_uppercase()
}

pub fn status_message(status: u16) -> String {
    http::StatusCode::from_u16(status)
        .ok()
        .and_then(|code| code.canonical_reason().map(str::to_string))
        .unwrap_or_default()
}

pub fn resolve_template_with<F>(source: &str, mut resolve_variable: F) -> String
where
    F: FnMut(&str) -> String,
{
    let Some(mut cursor) = source.find("${") else {
        return source.to_string();
    };

    let mut out = String::with_capacity(source.len());
    out.push_str(&source[..cursor]);

    while cursor < source.len() {
        let var_start = cursor + 2;
        let Some(relative_end) = source[var_start..].find('}') else {
            out.push_str(&source[cursor..]);
            return out;
        };
        let var_end = var_start + relative_end;
        out.push_str(&resolve_variable(&source[var_start..var_end]));

        let next_start = var_end + 1;
        if let Some(next_relative) = source[next_start..].find("${") {
            cursor = next_start + next_relative;
            out.push_str(&source[next_start..cursor]);
        } else {
            out.push_str(&source[next_start..]);
            return out;
        }
    }

    out
}

fn lookup_case_insensitive(values: &BTreeMap<String, String>, key: &str) -> String {
    values
        .get(key)
        .cloned()
        .or_else(|| {
            values
                .iter()
                .find(|(candidate, _)| candidate.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.clone())
        })
        .unwrap_or_default()
}

#[derive(Clone)]
struct HttpDimensionHandle {
    sender: mpsc::Sender<HttpDimensionEvent>,
    queue_capacity: usize,
    dropped_since_warning: Arc<AtomicU64>,
    last_warning_at_ms: Arc<AtomicI64>,
}

impl HttpDimensionHandle {
    fn new(sender: mpsc::Sender<HttpDimensionEvent>, queue_capacity: usize) -> Self {
        Self {
            sender,
            queue_capacity,
            dropped_since_warning: Arc::new(AtomicU64::new(0)),
            last_warning_at_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    fn try_enqueue(&self, event: HttpDimensionEvent) {
        match self.sender.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.record_drop("saturated");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.record_drop("closed");
            }
        }
    }

    fn record_drop(&self, reason: &str) {
        HTTP_DIMENSION_DROPPED.fetch_add(1, Ordering::Relaxed);
        crate::pipeline_metrics::increment(
            crate::pipeline_metrics::PipelineCounter::HttpDimensionDropped,
        );
        self.dropped_since_warning.fetch_add(1, Ordering::Relaxed);
        let now = unix_epoch_millis_now();
        let last = self.last_warning_at_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= HTTP_DIMENSION_WARNING_INTERVAL_MS as i64
            && self
                .last_warning_at_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let dropped = self.dropped_since_warning.swap(0, Ordering::Relaxed);
            tracing::warn!(
                "METRICS: http dimension queue {} dropped={} total_dropped={} queue_capacity={} available_capacity={}",
                reason,
                dropped,
                HTTP_DIMENSION_DROPPED.load(Ordering::Relaxed),
                self.queue_capacity,
                self.sender.capacity()
            );
        }
    }
}

fn unix_epoch_millis_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

pub fn init_http_dimension_worker(queue_capacity: usize) {
    let capacity = queue_capacity.max(1);
    let (sender, mut receiver) = mpsc::channel::<HttpDimensionEvent>(capacity);
    let handle = HttpDimensionHandle::new(sender, capacity);
    let _ = HTTP_DIMENSION_HANDLE.set(handle);
    tokio::spawn(async move {
        let mut buffer = Vec::with_capacity(1024);
        loop {
            let Some(event) = receiver.recv().await else {
                return;
            };
            record::record_http_dimension_event(event);
            while buffer.len() < 1024 {
                match receiver.try_recv() {
                    Ok(event) => buffer.push(event),
                    Err(_) => break,
                }
            }
            for event in buffer.drain(..) {
                record::record_http_dimension_event(event);
            }
        }
    });
}

/// Process-wide entry budget shared by every server's distinct-IP set.
/// Per-server capacities were derived from the server count at creation, so
/// each new server added another slice while old slices remained, making the
/// aggregate capacity grow with the harmonic sum of server generations.
struct DistinctIpBudget {
    capacity: usize,
    used: AtomicUsize,
}

impl DistinctIpBudget {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            used: AtomicUsize::new(0),
        }
    }

    fn try_charge(&self) -> bool {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.capacity).then_some(used + 1)
            })
            .is_ok()
    }

    fn refund(&self, count: usize) {
        let _ = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(count))
            });
    }

    fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    fn capacity(&self) -> usize {
        self.capacity
    }
}

static DISTINCT_IP_BUDGET: LazyLock<Arc<DistinctIpBudget>> = LazyLock::new(|| {
    Arc::new(DistinctIpBudget::new(
        crate::memory_governor::MEMORY_GOVERNOR.metrics_distinct_ip_capacity(),
    ))
});

/// Process-wide distinct-IP budget `(used_entries, capacity_entries)` for
/// node status. The budget is shared by every server.
pub fn distinct_ip_budget_status() -> (usize, usize) {
    (DISTINCT_IP_BUDGET.used(), DISTINCT_IP_BUDGET.capacity())
}

pub struct RuntimeDistinctIpTracker {
    ips: dashmap::DashSet<std::net::IpAddr>,
    budget: Arc<DistinctIpBudget>,
}

impl RuntimeDistinctIpTracker {
    fn new() -> Self {
        Self::with_budget(DISTINCT_IP_BUDGET.clone())
    }

    fn with_budget(budget: Arc<DistinctIpBudget>) -> Self {
        Self {
            ips: dashmap::DashSet::new(),
            budget,
        }
    }

    fn insert(&self, ip: std::net::IpAddr) {
        if self.ips.contains(&ip) {
            return;
        }
        if !self.budget.try_charge() {
            crate::pipeline_metrics::note_cardinality_drop(
                "runtime_distinct_ip_tracker",
                self.budget.used(),
                self.budget.capacity(),
            );
            return;
        }
        if !self.ips.insert(ip) {
            self.budget.refund(1);
        }
    }

    fn len(&self) -> usize {
        self.ips.len()
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.budget.capacity()
    }

    #[cfg(test)]
    fn with_capacity(capacity: usize) -> Self {
        Self::with_budget(Arc::new(DistinctIpBudget::new(capacity)))
    }
}

impl Drop for RuntimeDistinctIpTracker {
    fn drop(&mut self) {
        self.budget.refund(self.ips.len());
    }
}

static NEXT_SERVER_METRIC_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Metrics for a specific server (site)
pub struct ServerMetrics {
    /// Monotonic process-local identity. A removed site that is re-added gets
    /// a new instance even though its server id is unchanged, so consumer
    /// baselines cannot subtract one lifecycle's counters from another's.
    pub instance_id: u64,
    pub count_errors: AtomicU64,
    pub user_id: AtomicI64,
    pub user_plan_id: AtomicI64,
    pub plan_id: AtomicI64,
    pub total_requests: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub bytes_received: AtomicU64,
    pub cached_bytes: AtomicU64,
    pub count_cached_requests: AtomicU64,
    pub count_attack_requests: AtomicU64,
    pub attack_bytes: AtomicU64,
    pub origin_bytes_sent: AtomicU64,
    pub origin_bytes_received: AtomicU64,
    pub active_connections: AtomicI64,
    pub count_websocket_connections: AtomicU64,
    pub distinct_ips: RuntimeDistinctIpTracker,
}

pub struct ActiveRequestMetricsGuard {
    metrics: Arc<ServerMetrics>,
    finished: bool,
}

impl ActiveRequestMetricsGuard {
    pub fn new(metrics: Arc<ServerMetrics>) -> Self {
        metrics.active_connections.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics,
            finished: false,
        }
    }

    pub fn from_started(metrics: Arc<ServerMetrics>) -> Self {
        Self::new(metrics)
    }

    pub fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            decrement_active_connections(&self.metrics.active_connections);
        }
    }

    pub fn metrics(&self) -> &Arc<ServerMetrics> {
        &self.metrics
    }
}

impl Drop for ActiveRequestMetricsGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

impl Default for ServerMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerMetrics {
    pub fn new() -> Self {
        Self::with_distinct_ip_tracker(RuntimeDistinctIpTracker::new())
    }

    fn with_distinct_ip_tracker(distinct_ips: RuntimeDistinctIpTracker) -> Self {
        Self {
            instance_id: NEXT_SERVER_METRIC_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            count_errors: AtomicU64::new(0),
            user_id: AtomicI64::new(0),
            user_plan_id: AtomicI64::new(0),
            plan_id: AtomicI64::new(0),
            total_requests: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            cached_bytes: AtomicU64::new(0),
            count_cached_requests: AtomicU64::new(0),
            count_attack_requests: AtomicU64::new(0),
            attack_bytes: AtomicU64::new(0),
            origin_bytes_sent: AtomicU64::new(0),
            origin_bytes_received: AtomicU64::new(0),
            active_connections: AtomicI64::new(0),
            count_websocket_connections: AtomicU64::new(0),
            distinct_ips,
        }
    }

    pub fn snapshot(&self) -> ServerStatusSnapshot {
        ServerStatusSnapshot {
            instance_id: self.instance_id,
            server_id: 0, // set by caller
            count_errors: self.count_errors.load(Ordering::Relaxed),
            user_id: self.user_id.load(Ordering::Relaxed),
            user_plan_id: self.user_plan_id.load(Ordering::Relaxed),
            plan_id: self.plan_id.load(Ordering::Relaxed),
            total_requests: self.total_requests.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            cached_bytes: self.cached_bytes.load(Ordering::Relaxed),
            count_cached_requests: self.count_cached_requests.load(Ordering::Relaxed),
            count_attack_requests: self.count_attack_requests.load(Ordering::Relaxed),
            attack_bytes: self.attack_bytes.load(Ordering::Relaxed),
            origin_bytes_sent: self.origin_bytes_sent.load(Ordering::Relaxed),
            origin_bytes_received: self.origin_bytes_received.load(Ordering::Relaxed),
            active_connections: self.active_connections.load(Ordering::Relaxed).max(0),
            count_websocket_connections: self.count_websocket_connections.load(Ordering::Relaxed),
            count_ips: self.distinct_ips.len() as u64,
        }
    }
}

fn decrement_active_connections(counter: &AtomicI64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        if current > 0 {
            Some(current - 1)
        } else if current < 0 {
            Some(0)
        } else {
            None
        }
    });
}

#[derive(Clone)]
pub struct ServerStatusSnapshot {
    /// Process-local metric-instance identity; see `ServerMetrics::instance_id`.
    pub instance_id: u64,
    pub server_id: i64,
    pub count_errors: u64,
    pub user_id: i64,
    pub user_plan_id: i64,
    pub plan_id: i64,
    pub total_requests: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub cached_bytes: u64,
    pub count_cached_requests: u64,
    pub count_attack_requests: u64,
    pub attack_bytes: u64,
    pub origin_bytes_sent: u64,
    pub origin_bytes_received: u64,
    pub active_connections: i64,
    pub count_websocket_connections: u64,
    pub count_ips: u64,
}

impl ServerStatusSnapshot {
    pub fn total_bytes(&self) -> u64 {
        self.bytes_sent + self.bytes_received
    }

    /// Content identity of one sampled state. Retirement waits until all
    /// delta consumers have acknowledged the same fingerprinted state and no
    /// request still holds the metrics Arc, so late in-flight updates cannot
    /// be discarded after an earlier acknowledgement.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf29ce484222325u64;
        let values = [
            self.instance_id,
            self.server_id as u64,
            self.count_errors,
            self.user_id as u64,
            self.user_plan_id as u64,
            self.plan_id as u64,
            self.total_requests,
            self.bytes_sent,
            self.bytes_received,
            self.cached_bytes,
            self.count_cached_requests,
            self.count_attack_requests,
            self.attack_bytes,
            self.origin_bytes_sent,
            self.origin_bytes_received,
            self.active_connections as u64,
            self.count_websocket_connections,
            self.count_ips,
        ];
        for value in values {
            hash ^= value;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
}

/// The three consumers that report per-server counter deltas. Node and
/// performance snapshots read live state only and are not retirement barriers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricConsumer {
    Persistence,
    Bandwidth,
    Daily,
}

impl MetricConsumer {
    fn bit(self) -> u8 {
        1 << match self {
            Self::Persistence => 0,
            Self::Bandwidth => 1,
            Self::Daily => 2,
        }
    }
}

const METRIC_CONSUMER_COUNT: usize = 3;
const METRIC_ACK_ALL: u8 = (1 << METRIC_CONSUMER_COUNT) - 1;
const MAX_RETIRED_SERVER_METRICS: usize = 4_096;

/// One sample returned to a delta consumer. `metrics` is the exact Arc the
/// sample came from; consumers keep its Weak as the baseline identity and
/// drop the Arc before acknowledging so quiescence is observable.
pub struct ReportingSnapshot {
    pub metrics: Arc<ServerMetrics>,
    pub snapshot: ServerStatusSnapshot,
    pub retired: bool,
}

struct RetiredServerMetrics {
    server_id: i64,
    metrics: Arc<ServerMetrics>,
    acknowledged_mask: AtomicU8,
    acknowledged_fingerprints: [AtomicU64; METRIC_CONSUMER_COUNT],
}

impl RetiredServerMetrics {
    fn new(server_id: i64, metrics: Arc<ServerMetrics>) -> Self {
        Self {
            server_id,
            metrics,
            acknowledged_mask: AtomicU8::new(0),
            acknowledged_fingerprints: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

pub struct NodeMetrics {
    pub total_bytes_sent: AtomicU64,
    pub total_bytes_received: AtomicU64,
    pub servers: DashMap<i64, Arc<ServerMetrics>>,
    /// Entries removed from the live id map but not yet fully consumed by the
    /// persistence, bandwidth and daily delta reporters.
    retired_servers: DashMap<u64, RetiredServerMetrics>,
    retirement_deferred_total: AtomicU64,
    pub rpc: RpcMetrics,
    pub waf: WafRuntimeMetrics,
}

pub struct WafRuntimeMetrics {
    pub compiled_evaluations: AtomicU64,
    pub compiled_matches: AtomicU64,
    pub legacy_evaluations: AtomicU64,
    pub legacy_matches: AtomicU64,
    pub legacy_fallbacks: AtomicU64,
    pub request_body_buffers: AtomicU64,
    pub request_body_buffered_bytes: AtomicU64,
    pub rule_evaluations: AtomicU64,
    pub evaluation_latency_ns: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct WafRuntimeSnapshot {
    pub compiled_evaluations: u64,
    pub compiled_matches: u64,
    pub legacy_evaluations: u64,
    pub legacy_matches: u64,
    pub legacy_fallbacks: u64,
    pub request_body_buffers: u64,
    pub request_body_buffered_bytes: u64,
    pub rule_evaluations: u64,
    pub evaluation_latency_ns: u64,
}

impl WafRuntimeMetrics {
    pub fn new() -> Self {
        Self {
            compiled_evaluations: AtomicU64::new(0),
            compiled_matches: AtomicU64::new(0),
            legacy_evaluations: AtomicU64::new(0),
            legacy_matches: AtomicU64::new(0),
            legacy_fallbacks: AtomicU64::new(0),
            request_body_buffers: AtomicU64::new(0),
            request_body_buffered_bytes: AtomicU64::new(0),
            rule_evaluations: AtomicU64::new(0),
            evaluation_latency_ns: AtomicU64::new(0),
        }
    }

    pub fn record_compiled_evaluation(&self, matched: bool, elapsed: Duration) {
        self.compiled_evaluations.fetch_add(1, Ordering::Relaxed);
        if matched {
            self.compiled_matches.fetch_add(1, Ordering::Relaxed);
        }
        self.evaluation_latency_ns.fetch_add(
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    pub fn record_legacy_evaluation(&self, matched: bool, elapsed: Duration) {
        self.legacy_evaluations.fetch_add(1, Ordering::Relaxed);
        if matched {
            self.legacy_matches.fetch_add(1, Ordering::Relaxed);
        }
        self.evaluation_latency_ns.fetch_add(
            elapsed.as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }

    pub fn record_legacy_fallback(&self) {
        self.legacy_fallbacks.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_request_body_buffer(&self, bytes: usize) {
        self.request_body_buffers.fetch_add(1, Ordering::Relaxed);
        self.request_body_buffered_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn record_rule_evaluation(&self) {
        self.rule_evaluations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> WafRuntimeSnapshot {
        WafRuntimeSnapshot {
            compiled_evaluations: self.compiled_evaluations.load(Ordering::Relaxed),
            compiled_matches: self.compiled_matches.load(Ordering::Relaxed),
            legacy_evaluations: self.legacy_evaluations.load(Ordering::Relaxed),
            legacy_matches: self.legacy_matches.load(Ordering::Relaxed),
            legacy_fallbacks: self.legacy_fallbacks.load(Ordering::Relaxed),
            request_body_buffers: self.request_body_buffers.load(Ordering::Relaxed),
            request_body_buffered_bytes: self.request_body_buffered_bytes.load(Ordering::Relaxed),
            rule_evaluations: self.rule_evaluations.load(Ordering::Relaxed),
            evaluation_latency_ns: self.evaluation_latency_ns.load(Ordering::Relaxed),
        }
    }
}

impl Default for WafRuntimeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RpcMetrics {
    pub total_requests: AtomicU64,
    pub total_errors: AtomicU64,
    pub total_cost_ms: AtomicU64,
}

impl RpcMetrics {
    pub fn snapshot(&self) -> RpcStatusSnapshot {
        RpcStatusSnapshot {
            total_requests: self.total_requests.load(Ordering::Relaxed),
            total_errors: self.total_errors.load(Ordering::Relaxed),
            total_cost_ms: self.total_cost_ms.load(Ordering::Relaxed),
        }
    }
}

pub struct RpcStatusSnapshot {
    pub total_requests: u64,
    pub total_errors: u64,
    pub total_cost_ms: u64,
}

impl NodeMetrics {
    pub fn new() -> Self {
        Self {
            total_bytes_sent: AtomicU64::new(0),
            total_bytes_received: AtomicU64::new(0),
            servers: DashMap::with_shard_amount(32),
            retired_servers: DashMap::with_shard_amount(8),
            retirement_deferred_total: AtomicU64::new(0),
            rpc: RpcMetrics {
                total_requests: AtomicU64::new(0),
                total_errors: AtomicU64::new(0),
                total_cost_ms: AtomicU64::new(0),
            },
            waf: WafRuntimeMetrics::new(),
        }
    }

    /// Live snapshots for node/perf status. Retired entries stay out of these
    /// views while they drain through the delta consumers.
    pub fn take_snapshots(&self) -> Vec<(i64, ServerStatusSnapshot)> {
        self.servers
            .iter()
            .map(|entry| {
                let mut snap = entry.value().snapshot();
                snap.server_id = *entry.key();
                (*entry.key(), snap)
            })
            .collect()
    }

    /// Live plus retired samples for the three delta-reporting consumers.
    pub fn take_reporting_snapshots(&self) -> Vec<ReportingSnapshot> {
        let mut snapshots = Vec::with_capacity(self.servers.len() + self.retired_servers.len());
        let mut seen = HashSet::with_capacity(snapshots.capacity());
        for entry in self.servers.iter() {
            let instance_id = entry.value().instance_id;
            if !seen.insert(instance_id) {
                continue;
            }
            let mut snapshot = entry.value().snapshot();
            snapshot.server_id = *entry.key();
            snapshots.push(ReportingSnapshot {
                metrics: Arc::clone(entry.value()),
                snapshot,
                retired: false,
            });
        }
        for entry in self.retired_servers.iter() {
            let instance_id = entry.value().metrics.instance_id;
            if !seen.insert(instance_id) {
                continue;
            }
            let mut snapshot = entry.value().metrics.snapshot();
            snapshot.server_id = entry.value().server_id;
            snapshots.push(ReportingSnapshot {
                metrics: Arc::clone(&entry.value().metrics),
                snapshot,
                retired: true,
            });
        }
        snapshots
    }

    /// Move metric instances for removed server ids out of the live map.
    /// Candidates are ids that were present in an earlier configuration
    /// snapshot, not every id absent from the new one, so an unrelated test or
    /// pre-config request cannot be retired by a partial snapshot.
    ///
    /// Returns candidates that could not be retired because the bounded
    /// retirement set is full. The caller keeps and retries them on later
    /// configuration snapshots; data is retained rather than silently dropped.
    pub fn retire_servers_absent_from(
        &self,
        candidates: &HashSet<i64>,
        live_server_ids: &HashSet<i64>,
    ) -> HashSet<i64> {
        let mut deferred = HashSet::new();
        for &server_id in candidates {
            if server_id <= 0 || live_server_ids.contains(&server_id) {
                continue;
            }
            let Some(instance_id) = self.servers.get(&server_id).map(|entry| entry.instance_id)
            else {
                // No metric state exists for this id; nothing to retire.
                continue;
            };
            if self.retired_servers.len() >= MAX_RETIRED_SERVER_METRICS {
                deferred.insert(server_id);
                continue;
            }
            let Some((_, metrics)) = self
                .servers
                .remove_if(&server_id, |_, entry| entry.instance_id == instance_id)
            else {
                // A concurrent creation replaced it; the next snapshot retries.
                deferred.insert(server_id);
                continue;
            };
            self.retired_servers
                .insert(instance_id, RetiredServerMetrics::new(server_id, metrics));
        }
        if !deferred.is_empty() {
            self.retirement_deferred_total
                .fetch_add(deferred.len() as u64, Ordering::Relaxed);
        }
        deferred
    }

    /// Record that one consumer has processed `fingerprint` from this retired
    /// instance. The entry is released only when all three consumers have
    /// acknowledged the current fingerprint and no request holds the Arc.
    pub fn acknowledge_retired(
        &self,
        consumer: MetricConsumer,
        instance_id: u64,
        fingerprint: u64,
    ) {
        let Some(entry) = self.retired_servers.get(&instance_id) else {
            return;
        };
        if Arc::strong_count(&entry.metrics) > 1 {
            // An in-flight request (or this consumer's own sample Arc) still
            // owns the metrics; late counter updates must still be sampled.
            return;
        }
        let bit = consumer.bit();
        entry.acknowledged_fingerprints[bit.trailing_zeros() as usize]
            .store(fingerprint, Ordering::Release);
        let previous = entry.acknowledged_mask.fetch_or(bit, Ordering::AcqRel);
        if previous | bit != METRIC_ACK_ALL {
            return;
        }
        let mut current_snapshot = entry.metrics.snapshot();
        current_snapshot.server_id = entry.server_id;
        let current = current_snapshot.fingerprint();
        if entry
            .acknowledged_fingerprints
            .iter()
            .any(|value| value.load(Ordering::Acquire) != current)
        {
            return;
        }
        drop(entry);
        self.retired_servers.remove_if(&instance_id, |_, entry| {
            Arc::strong_count(&entry.metrics) == 1
                && entry.acknowledged_mask.load(Ordering::Acquire) == METRIC_ACK_ALL
                && entry
                    .acknowledged_fingerprints
                    .iter()
                    .all(|value| value.load(Ordering::Acquire) == current)
        });
    }

    pub fn retired_server_metrics_count(&self) -> usize {
        self.retired_servers.len()
    }

    pub fn retirement_deferred_total(&self) -> u64 {
        self.retirement_deferred_total.load(Ordering::Relaxed)
    }

    pub fn get_node_totals(&self) -> (u64, u64, i64) {
        let mut total_conns = 0;
        for entry in self.servers.iter() {
            total_conns += entry
                .value()
                .active_connections
                .load(Ordering::Relaxed)
                .max(0);
        }
        for entry in self.retired_servers.iter() {
            total_conns += entry
                .value()
                .metrics
                .active_connections
                .load(Ordering::Relaxed)
                .max(0);
        }
        (
            self.total_bytes_sent.load(Ordering::Relaxed),
            self.total_bytes_received.load(Ordering::Relaxed),
            total_conns,
        )
    }

    pub fn get_node_pressure(&self) -> f64 {
        f32::from_bits(CACHED_PRESSURE.load(Ordering::Relaxed)) as f64
    }
}

impl Default for NodeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

pub static METRICS: LazyLock<Arc<NodeMetrics>> = LazyLock::new(|| Arc::new(NodeMetrics::new()));

use std::sync::RwLock;

struct TimeCache<T> {
    data: RwLock<(i64, T)>,
}

impl<T: Clone + Default> TimeCache<T> {
    fn new() -> Self {
        Self {
            data: RwLock::new((0, T::default())),
        }
    }

    fn get_or_update<F>(&self, current_tick: i64, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        {
            let read = self.data.read().unwrap_or_else(|e| e.into_inner());
            if read.0 == current_tick {
                return read.1.clone();
            }
        }

        let mut write = self.data.write().unwrap_or_else(|e| e.into_inner());
        if write.0 == current_tick {
            return write.1.clone();
        }

        let new_val = f();
        *write = (current_tick, new_val.clone());
        new_val
    }
}

static DAY_CACHE: LazyLock<TimeCache<String>> = LazyLock::new(TimeCache::new);
static PERIOD_CACHE: LazyLock<TimeCache<i64>> = LazyLock::new(TimeCache::new);

fn get_current_day() -> String {
    let now = crate::utils::time::now_timestamp();
    let now_day = now / 86400;

    DAY_CACHE.get_or_update(now_day, || {
        crate::utils::time::now_local().format("%Y%m%d").to_string()
    })
}

fn get_current_5min_ts() -> i64 {
    let now = crate::utils::time::now_timestamp();
    let now_5min = now / 300;

    PERIOD_CACHE.get_or_update(now_5min, || {
        let dt = crate::utils::time::now_local();
        let minute_floor = (dt.minute() / 5) * 5;
        dt.with_second(0)
            .and_then(|d| d.with_minute(minute_floor))
            .map(|d| d.timestamp())
            .unwrap_or(now - (now % 300))
    })
}

pub mod record {
    use super::*;
    use std::net::IpAddr;

    pub fn get_or_create(server_id: i64) -> Arc<ServerMetrics> {
        METRICS
            .servers
            .entry(server_id)
            .or_insert_with(|| Arc::new(ServerMetrics::new()))
            .clone()
    }

    pub fn request_start(
        server_id: i64,
        remote_ip: &str,
        user_id: i64,
        user_plan_id: i64,
        plan_id: i64,
        cached_m: Option<&Arc<ServerMetrics>>,
        ip_recorded: bool,
    ) -> bool {
        let m = if let Some(m) = cached_m {
            (*m).clone()
        } else {
            get_or_create(server_id)
        };
        if user_id > 0 {
            m.user_id.store(user_id, Ordering::Relaxed);
        }
        if user_plan_id > 0 {
            m.user_plan_id.store(user_plan_id, Ordering::Relaxed);
        }
        if plan_id > 0 {
            m.plan_id.store(plan_id, Ordering::Relaxed);
        }
        m.total_requests.fetch_add(1, Ordering::Relaxed);
        m.active_connections.fetch_add(1, Ordering::Relaxed);
        if !ip_recorded {
            if let Ok(remote_ip) = remote_ip.parse::<IpAddr>() {
                let day = get_current_day();
                crate::metrics::daily::UNIQUE_IP_TRACKER.record(server_id, &day, remote_ip);
                m.distinct_ips.insert(remote_ip);
                return true;
            }
            return false;
        }
        false
    }

    /// Record request metadata without owning the active-connection lifetime.
    ///
    /// Callers that can be aborted must keep an `ActiveRequestMetricsGuard`
    /// beside their future and let that guard own the decrement.
    pub fn request_start_without_active(
        server_id: i64,
        remote_ip: &str,
        user_id: i64,
        user_plan_id: i64,
        plan_id: i64,
        cached_m: Option<&Arc<ServerMetrics>>,
        ip_recorded: bool,
    ) -> bool {
        let m = cached_m
            .cloned()
            .unwrap_or_else(|| get_or_create(server_id));
        if user_id > 0 {
            m.user_id.store(user_id, Ordering::Relaxed);
        }
        if user_plan_id > 0 {
            m.user_plan_id.store(user_plan_id, Ordering::Relaxed);
        }
        if plan_id > 0 {
            m.plan_id.store(plan_id, Ordering::Relaxed);
        }
        m.total_requests.fetch_add(1, Ordering::Relaxed);
        if !ip_recorded && let Ok(remote_ip) = remote_ip.parse::<IpAddr>() {
            let day = get_current_day();
            crate::metrics::daily::UNIQUE_IP_TRACKER.record(server_id, &day, remote_ip);
            m.distinct_ips.insert(remote_ip);
            return true;
        }
        false
    }

    pub fn active_request_guard(
        server_id: i64,
        cached_m: Option<&Arc<ServerMetrics>>,
    ) -> ActiveRequestMetricsGuard {
        let metrics = cached_m
            .cloned()
            .unwrap_or_else(|| get_or_create(server_id));
        ActiveRequestMetricsGuard::new(metrics)
    }

    pub fn request_end(
        server_id: i64,
        bytes_sent: u64,
        bytes_received: u64,
        is_cached: bool,
        is_attack: bool,
        is_websocket: bool,
        cached_m: Option<&Arc<ServerMetrics>>,
    ) {
        request_end_inner(
            server_id,
            bytes_sent,
            bytes_received,
            is_cached,
            is_attack,
            is_websocket,
            cached_m,
            true,
        );
    }

    pub fn request_end_without_active(
        server_id: i64,
        bytes_sent: u64,
        bytes_received: u64,
        is_cached: bool,
        is_attack: bool,
        is_websocket: bool,
        cached_m: Option<&Arc<ServerMetrics>>,
    ) {
        request_end_inner(
            server_id,
            bytes_sent,
            bytes_received,
            is_cached,
            is_attack,
            is_websocket,
            cached_m,
            false,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "dataplane operation carries explicit policy and connection context"
    )]
    fn request_end_inner(
        server_id: i64,
        bytes_sent: u64,
        bytes_received: u64,
        is_cached: bool,
        is_attack: bool,
        is_websocket: bool,
        cached_m: Option<&Arc<ServerMetrics>>,
        release_active: bool,
    ) {
        let m = if let Some(m) = cached_m {
            (*m).clone()
        } else {
            get_or_create(server_id)
        };
        if release_active {
            decrement_active_connections(&m.active_connections);
        }
        m.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
        m.bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
        crate::traffic_quota::record_outbound(server_id, bytes_sent);

        if is_cached {
            m.cached_bytes.fetch_add(bytes_sent, Ordering::Relaxed);
            m.count_cached_requests.fetch_add(1, Ordering::Relaxed);
        }

        if is_attack {
            m.attack_bytes.fetch_add(bytes_sent, Ordering::Relaxed);
            m.count_attack_requests.fetch_add(1, Ordering::Relaxed);
        }

        if is_websocket {
            m.count_websocket_connections
                .fetch_add(1, Ordering::Relaxed);
        }

        METRICS
            .total_bytes_sent
            .fetch_add(bytes_sent, Ordering::Relaxed);
        METRICS
            .total_bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
    }

    pub fn record_transfer(
        server_id: i64,
        bytes_sent: u64,
        bytes_received: u64,
        cached_m: Option<&Arc<ServerMetrics>>,
    ) {
        let m = if let Some(m) = cached_m {
            (*m).clone()
        } else {
            get_or_create(server_id)
        };
        m.bytes_sent.fetch_add(bytes_sent, Ordering::Relaxed);
        m.bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
        crate::traffic_quota::record_outbound(server_id, bytes_sent);

        METRICS
            .total_bytes_sent
            .fetch_add(bytes_sent, Ordering::Relaxed);
        METRICS
            .total_bytes_received
            .fetch_add(bytes_received, Ordering::Relaxed);
    }

    pub fn record_origin_traffic(
        server_id: i64,
        sent: u64,
        received: u64,
        cached_m: Option<&Arc<ServerMetrics>>,
    ) {
        let m = if let Some(m) = cached_m {
            (*m).clone()
        } else {
            get_or_create(server_id)
        };
        m.origin_bytes_sent.fetch_add(sent, Ordering::Relaxed);
        m.origin_bytes_received
            .fetch_add(received, Ordering::Relaxed);
    }

    pub fn record_rpc_call(cost_ms: u64, is_error: bool) {
        METRICS.rpc.total_requests.fetch_add(1, Ordering::Relaxed);
        if is_error {
            METRICS.rpc.total_errors.fetch_add(1, Ordering::Relaxed);
        }
        METRICS
            .rpc
            .total_cost_ms
            .fetch_add(cost_ms, Ordering::Relaxed);
    }

    pub fn record_http_dimensions(args: HttpDimensionsArgs<'_>) {
        record_dimensions(DimensionsArgs {
            category: crate::metrics::METRIC_CATEGORY_HTTP,
            server_id: args.server_id,
            client_ip: args.client_ip,
            domain: args.domain,
            user_agent: args.user_agent,
            bytes_sent: args.bytes_sent,
            bytes_received: args.bytes_received,
            cached_bytes: args.cached_bytes,
            waf_group_id: args.waf_group_id,
            waf_action: args.waf_action,
            cached_analyzed: args.cached_analyzed,
            request_context: args.request_context,
        });
    }

    pub fn record_network_dimensions(args: NetworkDimensionsArgs<'_>) {
        record_dimensions(DimensionsArgs {
            category: args.category,
            server_id: args.server_id,
            client_ip: args.client_ip,
            domain: args.domain,
            user_agent: args.user_agent,
            bytes_sent: args.bytes_sent,
            bytes_received: args.bytes_received,
            cached_bytes: 0,
            waf_group_id: 0,
            waf_action: None,
            cached_analyzed: None,
            request_context: Some(MetricRequestContext::network(
                args.domain,
                args.client_ip,
                args.user_agent,
                args.bytes_sent,
                args.bytes_received,
                args.status,
            )),
        });
    }

    fn record_dimensions(args: DimensionsArgs<'_>) {
        let DimensionsArgs {
            category,
            server_id,
            client_ip,
            domain,
            user_agent,
            bytes_sent,
            bytes_received,
            cached_bytes,
            waf_group_id,
            waf_action,
            cached_analyzed,
            request_context,
        } = args;
        if server_id <= 0 {
            return;
        }

        let category = crate::metrics::normalize_metric_category(category);
        let event = HttpDimensionEvent {
            category: Arc::from(category),
            server_id,
            client_ip,
            domain: if domain.is_empty() {
                Arc::clone(&EMPTY_ARC_STR)
            } else {
                Arc::from(domain)
            },
            user_agent: if user_agent.is_empty() {
                Arc::clone(&EMPTY_ARC_STR)
            } else {
                Arc::from(user_agent)
            },
            bytes_sent,
            bytes_received,
            cached_bytes,
            waf_group_id,
            waf_action: waf_action.map(|action| {
                if action.is_empty() {
                    Arc::clone(&EMPTY_ARC_STR)
                } else {
                    Arc::from(action)
                }
            }),
            analyzed: cached_analyzed.cloned(),
            request_context: Arc::new(request_context.unwrap_or_default()),
            created_at: get_current_5min_ts(),
        };

        if let Some(handle) = HTTP_DIMENSION_HANDLE.get() {
            handle.try_enqueue(event);
        } else {
            HTTP_DIMENSION_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_http_dimension_event(event: HttpDimensionEvent) {
        if event.server_id <= 0 {
            return;
        }

        let analyzed_owned;
        let analyzed = if let Some(ref analyzed) = event.analyzed {
            analyzed
        } else {
            analyzed_owned = crate::metrics::analyzer::analyze_request(
                event.client_ip,
                event.user_agent.as_ref(),
            );
            &analyzed_owned
        };

        let (country, country_id, province, province_id, city, city_id, provider) =
            analyzed.geo.as_ref().map_or_else(
                || {
                    (
                        Arc::clone(&EMPTY_ARC_STR),
                        0,
                        Arc::clone(&EMPTY_ARC_STR),
                        0,
                        Arc::clone(&EMPTY_ARC_STR),
                        0,
                        Arc::clone(&UNKNOWN_ARC_STR),
                    )
                },
                |geo| {
                    (
                        geo.country.clone(),
                        geo.country_id,
                        geo.region.clone(),
                        geo.region_id,
                        geo.city.clone(),
                        geo.city_id,
                        geo.provider.clone(),
                    )
                },
            );
        let provider_id = crate::metrics::storage::lookup_region_provider_id(event.client_ip);
        let key = crate::metrics::aggregator::AggregationKey {
            category: event.category.clone(),
            server_id: event.server_id,
            country,
            country_id,
            province,
            province_id,
            city,
            city_id,
            provider,
            browser: analyzed.browser.clone(),
            os: analyzed.os.clone(),
            waf_group_id: event.waf_group_id,
            waf_action: event
                .waf_action
                .clone()
                .unwrap_or_else(|| Arc::clone(&EMPTY_ARC_STR)),
            provider_id,
            browser_version: analyzed.browser_version.clone(),
            os_version: analyzed.os_version.clone(),
            request_attrs: Arc::new(event.request_context.values.clone()),
        };
        let is_attack = event.waf_action.is_some();

        crate::metrics::aggregator::METRIC_STAT_AGGREGATOR.record(
            key.clone(),
            event.bytes_sent,
            event.bytes_received,
            is_attack,
        );
        crate::metrics::top_ip::TOP_IP_TRACKER.record_addr(event.server_id, event.client_ip);

        if event.category.as_ref() == crate::metrics::METRIC_CATEGORY_HTTP {
            crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.record(
                key,
                event.bytes_sent,
                event.bytes_received,
                is_attack,
            );

            crate::metrics::daily::DAILY_DOMAIN_TRACKER.record(
                event.server_id,
                event.created_at,
                event.domain.as_ref(),
                event.bytes_sent,
                event.cached_bytes,
                1,
                if event.cached_bytes > 0 { 1 } else { 0 },
                if is_attack { 1 } else { 0 },
                if is_attack { event.bytes_sent } else { 0 },
            );
        }
    }
}

struct FlusherBaseline {
    metrics: Weak<ServerMetrics>,
    snapshot: ServerStatusSnapshot,
}

fn flusher_delta(
    current: &ServerStatusSnapshot,
    last: Option<&ServerStatusSnapshot>,
    field: fn(&ServerStatusSnapshot) -> u64,
) -> u64 {
    field(current).saturating_sub(last.map(field).unwrap_or(0))
}

pub async fn start_persistence_flusher() {
    use std::collections::HashMap;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut last_values: HashMap<u64, FlusherBaseline> = HashMap::new();
    let mut last_node_sent = 0u64;
    let mut last_node_recv = 0u64;

    loop {
        interval.tick().await;
        let metrics_state_bytes = crate::metrics::aggregator::METRIC_STAT_AGGREGATOR
            .approximate_bytes()
            .saturating_add(
                crate::metrics::aggregator::HTTP_REQUEST_STAT_AGGREGATOR.approximate_bytes(),
            )
            .saturating_add(crate::metrics::top_ip::TOP_IP_TRACKER.approximate_bytes())
            .saturating_add(crate::metrics::daily::DAILY_DOMAIN_TRACKER.approximate_bytes())
            .saturating_add(crate::metrics::daily::UNIQUE_IP_TRACKER.approximate_bytes());
        crate::memory_governor::MEMORY_GOVERNOR.set_metrics_aggregator_bytes(metrics_state_bytes);
        // Observability-only guard: the metrics maps are unbounded by design
        // today, so surface a rate-limited warning when their estimated
        // footprint becomes a material share of the node budget. No samples
        // are dropped here — capping requires an explicit policy decision.
        {
            static LAST_METRICS_WARN: AtomicI64 = AtomicI64::new(0);
            let snap = crate::memory_governor::MEMORY_GOVERNOR
                .snapshot(crate::memory_governor::MEMORY_GOVERNOR.pingora_worker_threads());
            let floor = 64u64 * 1024 * 1024;
            let limit = (snap.memory_total_bytes / 8).max(floor);
            let now = crate::utils::time::now_timestamp();
            let last = LAST_METRICS_WARN.load(std::sync::atomic::Ordering::Relaxed);
            if metrics_state_bytes > limit && now.saturating_sub(last) >= 600 {
                LAST_METRICS_WARN.store(now, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    metrics_state_bytes,
                    limit,
                    memory_total_bytes = snap.memory_total_bytes,
                    "METRICS_MEMORY: estimated metrics tracker footprint exceeds 1/8 of node memory budget; consider cardinality caps"
                );
            }
        }
        let mut updates = Vec::new();
        // Baselines are only committed after the Mace write succeeds; on a
        // failed write the next round recomputes the same deltas instead of
        // silently losing them.
        let mut new_baselines: Vec<(u64, FlusherBaseline)> = Vec::new();
        let mut retired_acks: Vec<(u64, u64)> = Vec::new();
        let now = crate::utils::time::now_timestamp();
        let period = (now / 300) * 300;

        for sample in METRICS.take_reporting_snapshots() {
            let current = sample.snapshot;
            let instance_id = current.instance_id;
            let server_id = current.server_id;
            let last = last_values.get(&instance_id).and_then(|baseline| {
                baseline
                    .metrics
                    .upgrade()
                    .filter(|metrics| Arc::ptr_eq(metrics, &sample.metrics))
                    .map(|_| &baseline.snapshot)
            });

            updates.push(crate::rpc::metrics::ServerMetricUpdate {
                server_id,
                user_id: current.user_id,
                user_plan_id: current.user_plan_id,
                plan_id: current.plan_id,
                total_requests: flusher_delta(&current, last, |s| s.total_requests),
                bytes_sent: flusher_delta(&current, last, |s| s.bytes_sent),
                bytes_received: flusher_delta(&current, last, |s| s.bytes_received),
                cached_bytes: flusher_delta(&current, last, |s| s.cached_bytes),
                count_cached_requests: flusher_delta(&current, last, |s| s.count_cached_requests),
                count_attack_requests: flusher_delta(&current, last, |s| s.count_attack_requests),
                attack_bytes: flusher_delta(&current, last, |s| s.attack_bytes),
                active_connections: current.active_connections,
                count_websocket_connections: flusher_delta(&current, last, |s| {
                    s.count_websocket_connections
                }),
                count_ips: current.count_ips,
            });

            if sample.retired {
                retired_acks.push((instance_id, current.fingerprint()));
            }
            new_baselines.push((
                instance_id,
                FlusherBaseline {
                    metrics: Arc::downgrade(&sample.metrics),
                    snapshot: current,
                },
            ));
        }

        let node_sent = METRICS.total_bytes_sent.load(Ordering::Relaxed);
        let node_recv = METRICS.total_bytes_received.load(Ordering::Relaxed);
        let node_sent_delta = node_sent - last_node_sent;
        let node_recv_delta = node_recv - last_node_recv;

        let cleanup = now - 86400;
        {
            let write_result = tokio::task::spawn_blocking(move || {
                let record_result = if !updates.is_empty() {
                    storage::STORAGE
                        .record_server_batch(period, updates, node_sent_delta, node_recv_delta)
                        .inspect_err(|err| {
                            tracing::error!(?err, "metric server batch persistence failed");
                        })
                } else {
                    Ok(())
                };
                // Cleanup has its own durable cursor and must advance even
                // while no samples were written this tick. Its failure must
                // not roll back the counters that were already persisted, and
                // a record failure must not stop retention either.
                if let Err(err) = storage::STORAGE.cleanup_old_stats(cleanup) {
                    tracing::error!(?err, "metric history cleanup failed");
                }
                record_result
            })
            .await;
            match write_result {
                Ok(Ok(())) => {
                    for (instance_id, baseline) in new_baselines {
                        last_values.insert(instance_id, baseline);
                    }
                    for (instance_id, fingerprint) in retired_acks {
                        METRICS.acknowledge_retired(
                            MetricConsumer::Persistence,
                            instance_id,
                            fingerprint,
                        );
                    }
                    last_values.retain(|_, baseline| baseline.metrics.strong_count() > 0);
                    last_node_sent = node_sent;
                    last_node_recv = node_recv;
                }
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "metrics persistence worker failed");
                }
                Err(err) => {
                    tracing::error!(error = %err, "metrics persistence worker failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn acknowledge_all(node: &NodeMetrics, consumer: MetricConsumer, id: u64, fingerprint: u64) {
        node.acknowledge_retired(consumer, id, fingerprint);
    }

    #[test]
    fn retirement_waits_for_consumers_quiescence_and_the_final_delta() {
        let node = NodeMetrics::new();
        let metrics = Arc::new(ServerMetrics::new());
        let instance_id = metrics.instance_id;
        node.servers.insert(7_710_001, Arc::clone(&metrics));

        let deferred =
            node.retire_servers_absent_from(&HashSet::from([7_710_001]), &HashSet::new());
        assert!(deferred.is_empty());
        assert!(!node.servers.contains_key(&7_710_001));
        assert_eq!(node.retired_server_metrics_count(), 1);
        drop(metrics);

        let sample = node
            .take_reporting_snapshots()
            .pop()
            .expect("retired instance is still reportable");
        assert!(sample.retired);
        let stale_fingerprint = sample.snapshot.fingerprint();
        let in_flight = Arc::clone(&sample.metrics);
        drop(sample);

        for consumer in [
            MetricConsumer::Persistence,
            MetricConsumer::Bandwidth,
            MetricConsumer::Daily,
        ] {
            acknowledge_all(&node, consumer, instance_id, stale_fingerprint);
        }
        assert_eq!(
            node.retired_server_metrics_count(),
            1,
            "an in-flight Arc must keep the final counters reportable"
        );

        in_flight.bytes_sent.fetch_add(1_024, Ordering::Relaxed);
        drop(in_flight);

        let samples = node.take_reporting_snapshots();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].snapshot.bytes_sent, 1_024);
        let final_fingerprint = samples[0].snapshot.fingerprint();
        assert_ne!(final_fingerprint, stale_fingerprint);
        drop(samples);

        for consumer in [
            MetricConsumer::Persistence,
            MetricConsumer::Bandwidth,
            MetricConsumer::Daily,
        ] {
            acknowledge_all(&node, consumer, instance_id, final_fingerprint);
        }
        assert_eq!(node.retired_server_metrics_count(), 0);
    }

    #[test]
    fn re_added_server_id_uses_a_distinct_metric_instance() {
        let node = NodeMetrics::new();
        let old = Arc::new(ServerMetrics::new());
        let old_instance = old.instance_id;
        node.servers.insert(7_710_002, Arc::clone(&old));
        node.retire_servers_absent_from(&HashSet::from([7_710_002]), &HashSet::new());

        let new = Arc::new(ServerMetrics::new());
        let new_instance = new.instance_id;
        node.servers.insert(7_710_002, Arc::clone(&new));
        let deferred = node
            .retire_servers_absent_from(&HashSet::from([7_710_002]), &HashSet::from([7_710_002]));
        assert!(deferred.is_empty());
        assert!(node.servers.contains_key(&7_710_002));

        let instances: HashSet<u64> = node
            .take_reporting_snapshots()
            .into_iter()
            .filter(|sample| sample.snapshot.server_id == 7_710_002)
            .map(|sample| sample.snapshot.instance_id)
            .collect();
        assert_eq!(instances, HashSet::from([old_instance, new_instance]));
        assert_ne!(old_instance, new_instance);
        assert_eq!(
            node.take_snapshots().len(),
            1,
            "node/perf views only expose the live instance"
        );
    }

    #[test]
    fn retirement_cap_defers_instead_of_dropping_metric_state() {
        let node = NodeMetrics::new();
        let ids: HashSet<i64> = (0..=MAX_RETIRED_SERVER_METRICS)
            .map(|offset| 7_720_000 + offset as i64)
            .collect();
        for id in &ids {
            node.servers.insert(*id, Arc::new(ServerMetrics::new()));
        }
        let deferred = node.retire_servers_absent_from(&ids, &HashSet::new());
        assert_eq!(
            node.retired_server_metrics_count(),
            MAX_RETIRED_SERVER_METRICS
        );
        assert_eq!(node.servers.len(), 1);
        assert_eq!(deferred.len(), 1);
        assert_eq!(node.retirement_deferred_total(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::MetricRequestContext;
    use std::net::IpAddr;
    use std::sync::Arc;

    #[test]
    fn metric_request_context_resolves_go_style_keys() {
        let mut ctx = MetricRequestContext::default();
        ctx.insert("status", "403");
        ctx.insert("host", "cdn.example.com");
        ctx.insert("requestMethod", "get");
        ctx.insert("requestPath", "/index.html");
        ctx.insert("requestURI", "/index.html?page=1");
        ctx.insert("header.User-Agent", "curl/8");
        ctx.insert("cookie.sid", "abc");
        ctx.insert("arg.page", "1");
        ctx.insert("response.header.Content-Type", "text/html");

        assert_eq!(ctx.resolve_key("${status}"), "403");
        assert_eq!(ctx.resolve_key("${host}"), "cdn.example.com");
        assert_eq!(ctx.resolve_key("${method}"), "GET");
        assert_eq!(ctx.resolve_key("${request.method}"), "GET");
        assert_eq!(ctx.resolve_key("${requestMethod}"), "GET");
        assert_eq!(ctx.resolve_key("${path}"), "/index.html");
        assert_eq!(ctx.resolve_key("${request.path}"), "/index.html");
        assert_eq!(ctx.resolve_key("${uri}"), "/index.html?page=1");
        assert_eq!(ctx.resolve_key("${request.uri}"), "/index.html?page=1");
        assert_eq!(ctx.resolve_key("${header.user-agent}"), "curl/8");
        assert_eq!(ctx.resolve_key("${http.User-Agent}"), "curl/8");
        assert_eq!(ctx.resolve_key("${cookie.sid}"), "abc");
        assert_eq!(ctx.resolve_key("${arg.page}"), "1");
        assert_eq!(ctx.resolve_key("${response.contentType}"), "text/html");
        assert_eq!(
            ctx.resolve_key("${response.header.content-type}"),
            "text/html"
        );
        assert_eq!(
            ctx.resolve_key("${host}-${status}-${header.user-agent}"),
            "cdn.example.com-403-curl/8"
        );
    }

    #[test]
    fn metric_request_context_preserves_unknown_simple_vars() {
        let ctx = MetricRequestContext::default();

        assert_eq!(ctx.resolve_key("${unknownVar}"), "${unknownVar}");
        assert_eq!(ctx.resolve_key("${unknown.prefix}"), "");
        assert_eq!(ctx.resolve_key("literal"), "literal");
    }

    #[test]
    fn network_metric_context_resolves_status_and_common_keys() {
        let ctx = MetricRequestContext::network(
            "edge.example.com",
            "203.0.113.9".parse().expect("valid ip"),
            "-",
            1234,
            56,
            502,
        );

        assert_eq!(ctx.resolve_key("${status}"), "502");
        assert_eq!(ctx.resolve_key("${statusMessage}"), "Bad Gateway");
        assert_eq!(ctx.resolve_key("${host}"), "edge.example.com");
        assert_eq!(ctx.resolve_key("${serverName}"), "edge.example.com");
        assert_eq!(ctx.resolve_key("${remoteAddr}"), "203.0.113.9");
        assert_eq!(ctx.resolve_key("${bytesSent}"), "1234");
        assert_eq!(ctx.resolve_key("${requestLength}"), "56");
    }

    #[test]
    fn runtime_distinct_ip_tracker_fail_closed_at_capacity() {
        let tracker = super::RuntimeDistinctIpTracker::with_capacity(512);
        let capacity = tracker.capacity();

        for i in 0..capacity.saturating_add(128) {
            let ip = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
            tracker.insert(ip);
        }
        assert!(
            tracker.len() <= capacity,
            "distinct IP tracker must not grow beyond capacity ({capacity}), got {}",
            tracker.len()
        );
    }

    #[test]
    fn distinct_ip_trackers_share_one_budget_and_refund_on_drop() {
        let budget = Arc::new(super::DistinctIpBudget::new(2));
        let first = super::RuntimeDistinctIpTracker::with_budget(budget.clone());
        let second = super::RuntimeDistinctIpTracker::with_budget(budget.clone());

        first.insert(IpAddr::from([10, 0, 0, 1]));
        second.insert(IpAddr::from([10, 0, 0, 2]));
        first.insert(IpAddr::from([10, 0, 0, 3]));
        assert_eq!(budget.used(), 2);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(budget.used() <= budget.capacity());

        drop(first);
        assert_eq!(budget.used(), 1);
        second.insert(IpAddr::from([10, 0, 0, 3]));
        assert_eq!(second.len(), 2);
        assert_eq!(budget.used(), 2);
    }

    #[test]
    fn active_request_guard_drops_to_exact_baseline() {
        let metrics = std::sync::Arc::new(super::ServerMetrics::new());
        assert_eq!(
            metrics
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        {
            let _guard = super::ActiveRequestMetricsGuard::new(metrics.clone());
            assert_eq!(
                metrics
                    .active_connections
                    .load(std::sync::atomic::Ordering::Relaxed),
                1
            );
        }
        assert_eq!(
            metrics
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn active_request_guard_finish_is_idempotent() {
        let metrics = std::sync::Arc::new(super::ServerMetrics::new());
        let mut guard = super::ActiveRequestMetricsGuard::new(metrics.clone());
        guard.finish();
        guard.finish();
        assert_eq!(
            metrics
                .active_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn active_request_guard_keeps_old_arc_after_registry_replacement() {
        let old = super::record::get_or_create(9_001);
        let mut guard = super::ActiveRequestMetricsGuard::new(old.clone());
        super::METRICS.servers.remove(&9_001);
        let new = super::record::get_or_create(9_001);
        guard.finish();
        assert_eq!(
            old.active_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            new.active_connections
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn shadow_connection_metrics_track_distinct_lifecycles() {
        let before = super::shadow_connection_metrics();
        let transport = super::transport_metrics_guard(super::ShadowTransportKind::DownstreamTcp);
        let pending = super::pending_request_metrics_guard();
        let forwarding = super::forwarding_metrics_guard();
        let during = super::shadow_connection_metrics();

        assert_eq!(
            during.downstream_transports,
            before.downstream_transports + 1
        );
        assert_eq!(during.pending_requests, before.pending_requests + 1);
        assert_eq!(during.forwarding_requests, before.forwarding_requests + 1);

        drop(forwarding);
        drop(pending);
        drop(transport);
        assert_eq!(super::shadow_connection_metrics(), before);
    }
}
