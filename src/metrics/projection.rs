//! Generation-scoped custom metric projection aggregator.
//!
//! A projection stores only the dimensions a configured item actually asks
//! for: `(item, version, event window, server, resolved key values)` plus the
//! additive counters. It never retains the full request attribute map, a
//! request id, headers, cookies, or a per-request sample. Request-id keys that
//! the user explicitly configures still produce distinct projections; they are
//! bounded by the same cardinality and byte ceilings as every other key.
//!
//! Charge ownership is explicit and transferable. A charged row keeps its
//! charge when it leaves the aggregator and until the reporter has durably
//! checkpointed and spooled it, so a flush cannot temporarily double the
//! admitted footprint.

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProjectionCounters {
    pub count: i64,
    pub count_attack: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub attack_bytes: i64,
}

impl ProjectionCounters {
    pub fn add(&mut self, other: &Self) {
        self.count = self.count.saturating_add(other.count);
        self.count_attack = self.count_attack.saturating_add(other.count_attack);
        self.bytes_sent = self.bytes_sent.saturating_add(other.bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(other.bytes_received);
        self.attack_bytes = self.attack_bytes.saturating_add(other.attack_bytes);
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProjectionKey {
    pub item_id: i64,
    pub version: i32,
    pub time_key: Arc<str>,
    pub window_start: i64,
    pub window_end: i64,
    pub server_id: i64,
    pub keys: Arc<[String]>,
}

#[derive(Debug)]
pub struct ProjectionEntry {
    pub counters: ProjectionCounters,
    /// Latest event time in this period window. The snapshot reports a closed
    /// wire window ending just after the last real event instead of claiming
    /// the whole future period; the period key remains the identity.
    pub last_event_time_millis: i64,
    /// Bytes charged in the shared pool for this key. Transferred to the
    /// snapshot store when the row is merged; released only when the snapshot
    /// row retires.
    pub charge: u64,
}

#[derive(Debug)]
pub struct ProjectionDrain {
    pub rows: Vec<(ProjectionKey, ProjectionEntry)>,
    /// Charge that must be released by the caller once ownership of these rows
    /// has been durably transferred (or the rows discarded explicitly).
    pub charged_bytes: u64,
}

/// Process-wide charge pool shared by plan aggregators, the durable snapshot
/// store and the one serialized chunk in flight. The snapshot store releases
/// a row's charge when the row retires.
pub struct MetricChargePool {
    used: AtomicU64,
}

impl MetricChargePool {
    const fn new() -> Self {
        Self {
            used: AtomicU64::new(0),
        }
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    pub fn try_charge(&self, bytes: u64) -> bool {
        let (_, budget) = crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            if used.saturating_add(bytes) > budget {
                crate::pipeline_metrics::note_cardinality_drop(
                    "metric_projection_bytes",
                    used as usize,
                    budget as usize,
                );
                return false;
            }
            match self.used.compare_exchange_weak(
                used,
                used.saturating_add(bytes),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => used = current,
            }
        }
    }

    pub fn refund(&self, bytes: u64) {
        let _ = self
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }
}

pub static METRIC_PROJECTION_POOL: MetricChargePool = MetricChargePool::new();

static DROPPED_PROJECTIONS: AtomicU64 = AtomicU64::new(0);

pub fn dropped_projections() -> u64 {
    DROPPED_PROJECTIONS.load(Ordering::Acquire)
}

pub struct MetricProjectionAggregator {
    /// Swapping the whole map on drain drops the old DashMap capacity instead
    /// of leaving one allocation per historical generation behind.
    data: RwLock<DashMap<ProjectionKey, ProjectionEntry>>,
    entry_count: AtomicUsize,
    dropped: AtomicU64,
    generation: u64,
    /// Test-only local charge pool. Parallel unit tests must not share the
    /// process-global accounting, which other tests legitimately populate.
    #[cfg(test)]
    pool: Option<Arc<MetricChargePool>>,
}

impl MetricProjectionAggregator {
    pub fn new(generation: u64) -> Self {
        Self {
            data: RwLock::new(DashMap::with_shard_amount(32)),
            entry_count: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            generation,
            #[cfg(test)]
            pool: None,
        }
    }

    /// Test-only constructor that isolates charge accounting in a local pool.
    #[cfg(test)]
    fn with_pool(generation: u64, pool: Arc<MetricChargePool>) -> Self {
        Self {
            data: RwLock::new(DashMap::with_shard_amount(32)),
            entry_count: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            generation,
            pool: Some(pool),
        }
    }

    /// Production always uses the process-wide pool; the local-pool branch
    /// exists only in test builds.
    #[cfg(test)]
    fn charge_pool(&self) -> &MetricChargePool {
        self.pool.as_deref().unwrap_or(&METRIC_PROJECTION_POOL)
    }

    #[cfg(not(test))]
    fn charge_pool(&self) -> &MetricChargePool {
        &METRIC_PROJECTION_POOL
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn entries(&self) -> usize {
        self.entry_count.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.data.read().is_empty()
    }

    /// Either the whole projection is admitted and counted, or it is rejected
    /// and reported. There is no path that adds the count but drops the
    /// dimensions (the old samples-vs-entry split).
    pub fn record(
        &self,
        key: ProjectionKey,
        delta: ProjectionCounters,
        event_time_millis: i64,
    ) -> bool {
        let (capacity, _) = crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        self.record_with_capacity(key, delta, event_time_millis, capacity)
    }

    fn record_with_capacity(
        &self,
        key: ProjectionKey,
        delta: ProjectionCounters,
        event_time_millis: i64,
        capacity: usize,
    ) -> bool {
        let data = self.data.read();
        match data.entry(key) {
            Entry::Occupied(mut occupied) => {
                let entry = occupied.get_mut();
                entry.counters.add(&delta);
                entry.last_event_time_millis = entry
                    .last_event_time_millis
                    .max(event_time_millis);
                true
            }
            Entry::Vacant(vacant) => {
                // While the reporter cannot open its checkpoint, nothing can
                // drain the shared pool. Reject new identities (counted and
                // visible) instead of filling the pool to its cap; existing
                // keys still aggregate without new memory.
                if crate::rpc::metric_stat_cache::metric_snapshot_store_unavailable() {
                    crate::pipeline_metrics::note_cardinality_drop(
                        "metric_projection_store_unavailable",
                        self.entry_count.load(Ordering::Relaxed),
                        capacity,
                    );
                    self.dropped.fetch_add(1, Ordering::AcqRel);
                    DROPPED_PROJECTIONS.fetch_add(1, Ordering::AcqRel);
                    return false;
                }
                if self.entry_count.load(Ordering::Acquire) >= capacity {
                    crate::pipeline_metrics::note_cardinality_drop(
                        "metric_projection_entries",
                        self.entry_count.load(Ordering::Relaxed),
                        capacity,
                    );
                    self.dropped.fetch_add(1, Ordering::AcqRel);
                    DROPPED_PROJECTIONS.fetch_add(1, Ordering::AcqRel);
                    return false;
                }
                let charge = projection_cost(vacant.key());
                if !self.charge_pool().try_charge(charge) {
                    self.dropped.fetch_add(1, Ordering::AcqRel);
                    DROPPED_PROJECTIONS.fetch_add(1, Ordering::AcqRel);
                    return false;
                }
                self.entry_count.fetch_add(1, Ordering::AcqRel);
                vacant.insert(ProjectionEntry {
                    counters: delta,
                    last_event_time_millis: event_time_millis,
                    charge,
                });
                true
            }
        }
    }

    /// Remove every row without refunding its charge. The caller owns the
    /// returned charge and must either transfer it into the durable snapshot
    /// store or release it explicitly.
    pub fn drain(&self) -> ProjectionDrain {
        // Take the whole map under the write lock and iterate it by value. The
        // old map (including its shard capacity) is dropped as soon as the
        // drain is consumed, and no key is cloned just to remove it.
        let old = {
            let mut data = self.data.write();
            let old = std::mem::replace(&mut *data, DashMap::with_shard_amount(32));
            // Reset the counter under the same write lock that swapped the
            // map. Otherwise a worker admitting a row into the fresh map
            // between the swap and the reset would be erased from the count.
            self.entry_count.store(0, Ordering::Release);
            old
        };
        let mut rows = Vec::with_capacity(old.len());
        let mut charged_bytes = 0u64;
        for (key, entry) in old {
            charged_bytes = charged_bytes.saturating_add(entry.charge);
            rows.push((key, entry));
        }
        ProjectionDrain {
            rows,
            charged_bytes,
        }
    }

    /// Release charge for rows whose deltas were merged into an existing
    /// snapshot key (no new snapshot allocation) or explicitly discarded.
    pub fn release_charge(&self, bytes: u64) {
        self.charge_pool().refund(bytes);
    }

    pub fn memory_estimate(&self) -> u64 {
        self.data
            .read()
            .iter()
            .map(|entry| entry.value().charge)
            .fold(0u64, u64::saturating_add)
    }
}

fn projection_cost(key: &ProjectionKey) -> u64 {
    let mut bytes = (std::mem::size_of::<ProjectionKey>() + std::mem::size_of::<ProjectionEntry>())
        as u64;
    let mut key_heap = key.time_key.len() as u64;
    for part in key.keys.iter() {
        key_heap = key_heap.saturating_add(part.capacity() as u64 + 16);
    }
    // The snapshot store owns a BTreeMap key and a duplicate row key, both as
    // deep String/Vec copies. Reserve that larger form here so the charge
    // transfers without a second admission gate.
    bytes = bytes.saturating_add(key_heap.saturating_mul(2));
    bytes.saturating_add(256)
}

/// Dimensions derived from the analysis result (geo, UA, WAF). Shared strings
/// keep per-event clones cheap and never include request headers or cookies.
#[derive(Clone, Debug)]
pub struct ResolvedDimensions {
    pub country: Arc<str>,
    pub country_id: i64,
    pub province: Arc<str>,
    pub province_id: i64,
    pub city: Arc<str>,
    pub city_id: i64,
    pub provider: Arc<str>,
    pub provider_id: i64,
    pub browser: Arc<str>,
    pub browser_version: Arc<str>,
    pub os: Arc<str>,
    pub os_version: Arc<str>,
    pub waf_group_id: i64,
    pub waf_action: Arc<str>,
}

impl Default for ResolvedDimensions {
    fn default() -> Self {
        Self {
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(""),
            city_id: 0,
            provider: Arc::from("Unknown"),
            provider_id: 0,
            browser: Arc::from(""),
            browser_version: Arc::from(""),
            os: Arc::from(""),
            os_version: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
        }
    }
}

impl ResolvedDimensions {
    /// Resolve an analysis-derived variable. `resolved` is what the generic
    /// request-map resolver produced; an empty or unresolved value falls back
    /// to the analysis value, exactly like the previous AggregationKey path.
    pub fn resolve_variable(&self, var_name: &str, resolved: String) -> String {
        match var_name {
            "country" | "geo.country.name" => self.country.to_string(),
            "province" | "geo.province.name" => self.province.to_string(),
            "city" | "geo.city.name" => self.city.to_string(),
            "provider" | "isp.name" => self.provider.to_string(),
            "browser" | "browser.name" => self.browser.to_string(),
            "browser.version" => self.browser_version.to_string(),
            "os" | "browser.os.name" => self.os.to_string(),
            "browser.os.version" => self.os_version.to_string(),
            "wafGroup" => self.waf_group_id.to_string(),
            "wafAction" => self.waf_action.to_string(),
            "geo.country.id" => self.country_id.to_string(),
            "geo.province.id" => self.province_id.to_string(),
            "geo.city.id" => self.city_id.to_string(),
            "isp.id" => self.provider_id.to_string(),
            "geo.town.name" => String::new(),
            "geo.town.id" => "0".to_string(),
            _ => resolved,
        }
    }
}

/// The rows the reporter asks the snapshot store to aggregate. Items and
/// versions are already resolved by the plan; this type only carries data.
pub type ProjectionRows = Vec<(ProjectionKey, ProjectionEntry)>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn key(city: &str, request_id: &str) -> ProjectionKey {
        ProjectionKey {
            item_id: 1,
            version: 1,
            time_key: Arc::from("202610311200"),
            window_start: 1_790_000_000,
            window_end: 1_790_000_060,
            server_id: 7,
            keys: Arc::from(vec![
                format!("city={city}"),
                format!("rid={request_id}"),
            ]),
        }
    }

    fn aggregator_with_pool(generation: u64) -> (MetricProjectionAggregator, Arc<MetricChargePool>) {
        let pool = Arc::new(MetricChargePool::new());
        (
            MetricProjectionAggregator::with_pool(generation, Arc::clone(&pool)),
            pool,
        )
    }

    fn delta(value: i64) -> ProjectionCounters {
        ProjectionCounters {
            count: 1,
            count_attack: 0,
            bytes_sent: value,
            bytes_received: 0,
            attack_bytes: 0,
        }
    }

    #[test]
    fn varying_request_ids_are_bounded_by_the_entry_cap() {
        let (aggregator, pool) = aggregator_with_pool(1);
        for index in 0..100_000 {
            let _ = aggregator.record_with_capacity(
                key("Paris", &format!("request-{index}")),
                delta(1),
                index as i64,
                128,
            );
        }
        assert!(aggregator.entries() <= 128);
        assert!(aggregator.dropped() > 0);
        let rows = aggregator.drain();
        aggregator.release_charge(rows.charged_bytes);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn stable_country_keys_merge_while_different_request_ids_stay_distinct() {
        let (aggregator, pool) = aggregator_with_pool(1);
        assert!(aggregator.record(key("Paris", "a"), delta(10), 1_790_000_000_000));
        assert!(aggregator.record(key("Paris", "a"), delta(20), 1_790_000_001_000));
        assert!(aggregator.record(key("Paris", "b"), delta(30), 1_790_000_002_000));
        assert_eq!(aggregator.entries(), 2);
        let rows = aggregator.drain();
        let total: i64 = rows
            .rows
            .iter()
            .map(|(_, entry)| entry.counters.bytes_sent)
            .sum();
        assert_eq!(total, 60);
        aggregator.release_charge(rows.charged_bytes);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn every_event_time_lands_in_its_own_minute_window() {
        let (aggregator, pool) = aggregator_with_pool(1);
        let mut first = key("Paris", "a");
        first.time_key = Arc::from("202610311247");
        first.window_start = 1_790_000_000;
        let mut second = key("Paris", "a");
        second.time_key = Arc::from("202610311248");
        second.window_start = 1_790_000_060;
        assert!(aggregator.record(first, delta(1), 1_790_000_000_000));
        assert!(aggregator.record(second, delta(2), 1_790_000_060_000));
        assert_eq!(aggregator.entries(), 2);
        let rows = aggregator.drain();
        aggregator.release_charge(rows.charged_bytes);
        assert_eq!(pool.used(), 0);
    }
}
