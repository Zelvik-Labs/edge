//! Built-in HTTP request-statistic aggregator.
//!
//! Keys carry only the dimensions the request-stat consumer reads (server,
//! geo ids, ISP, OS, browser, WAF). They never carry request attributes,
//! request ids, headers or per-request samples: the custom metric pipeline has
//! its own generation-scoped projection aggregator in `super::projection`.

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Clone)]
pub struct AggregationKey {
    pub category: Arc<str>,
    pub server_id: i64,
    pub window_start: i64,
    pub country: Arc<str>,
    pub country_id: i64,
    pub province: Arc<str>,
    pub province_id: i64,
    pub city: Arc<str>,
    pub city_id: i64,
    pub provider: Arc<str>,
    pub browser: Arc<str>,
    pub os: Arc<str>,
    pub waf_group_id: i64,
    pub waf_action: Arc<str>,
    pub provider_id: i64,
    pub browser_version: Arc<str>,
    pub os_version: Arc<str>,
}

impl PartialEq for AggregationKey {
    fn eq(&self, other: &Self) -> bool {
        self.category == other.category
            && self.server_id == other.server_id
            && self.window_start == other.window_start
            && self.country == other.country
            && self.country_id == other.country_id
            && self.province == other.province
            && self.province_id == other.province_id
            && self.city == other.city
            && self.city_id == other.city_id
            && self.provider == other.provider
            && self.browser == other.browser
            && self.os == other.os
            && self.waf_group_id == other.waf_group_id
            && self.waf_action == other.waf_action
            && self.provider_id == other.provider_id
            && self.browser_version == other.browser_version
            && self.os_version == other.os_version
    }
}

impl Eq for AggregationKey {}

impl Hash for AggregationKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.category.hash(state);
        self.server_id.hash(state);
        self.window_start.hash(state);
        self.country.hash(state);
        self.country_id.hash(state);
        self.province.hash(state);
        self.province_id.hash(state);
        self.city.hash(state);
        self.city_id.hash(state);
        self.provider.hash(state);
        self.browser.hash(state);
        self.os.hash(state);
        self.waf_group_id.hash(state);
        self.waf_action.hash(state);
        self.provider_id.hash(state);
        self.browser_version.hash(state);
        self.os_version.hash(state);
    }
}

#[derive(Debug, Default, Clone)]
pub struct AggregatedValue {
    pub count: i64,
    pub count_attack: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub attack_bytes: i64,
    /// Bytes charged for this entry.
    pub(crate) estimated_bytes: u64,
}

impl AggregatedValue {
    fn add(&mut self, bytes_sent: i64, bytes_received: i64, is_attack: bool) {
        self.count += 1;
        self.bytes_sent += bytes_sent;
        self.bytes_received += bytes_received;
        if is_attack {
            self.count_attack += 1;
            self.attack_bytes += bytes_sent;
        }
    }
}

/// Estimate of live aggregator allocations. Shared `Arc` allocations may be
/// counted more than once; allocator slack and container implementation details
/// are not observable here. This is neither RSS nor an enforced allocation
/// budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricAggregatorMemoryEstimate {
    pub entries: usize,
    pub live_bytes: u64,
    pub flush_temporary_bytes: u64,
}

fn estimated_entry_bytes(key: &AggregationKey, value: &AggregatedValue) -> (u64, u64) {
    let dimensions = [
        &key.category,
        &key.country,
        &key.province,
        &key.city,
        &key.provider,
        &key.browser,
        &key.os,
        &key.waf_action,
        &key.browser_version,
        &key.os_version,
    ];
    let mut live = (std::mem::size_of::<AggregationKey>()
        + std::mem::size_of::<AggregatedValue>()
        + 32) as u64;
    for dimension in dimensions {
        live =
            live.saturating_add(dimension.len() as u64 + 2 * std::mem::size_of::<usize>() as u64);
    }
    let _ = value;
    let temporary = (std::mem::size_of::<AggregationKey>()
        + 2 * std::mem::size_of::<(AggregationKey, AggregatedValue)>()) as u64;
    (live, temporary)
}

pub struct MetricAggregator {
    pub data: DashMap<AggregationKey, AggregatedValue>,
    used_bytes: AtomicU64,
    entry_count: AtomicUsize,
}

impl Default for MetricAggregator {
    fn default() -> Self {
        Self::new()
    }
}

/// Rows removed from an aggregator whose charge is still held by the caller.
/// The caller must either hand them to the next owner or call
/// [`MetricAggregator::release_charge`] after they are durably persisted.
pub struct AggregatorDrain {
    pub rows: Vec<(AggregationKey, AggregatedValue)>,
    pub charged_bytes: u64,
}

impl MetricAggregator {
    pub fn new() -> Self {
        Self {
            data: DashMap::with_shard_amount(32),
            used_bytes: AtomicU64::new(0),
            entry_count: AtomicUsize::new(0),
        }
    }

    fn try_charge(&self, bytes: u64, budget: u64) -> bool {
        let mut used = self.used_bytes.load(Ordering::Acquire);
        loop {
            if used.saturating_add(bytes) > budget {
                crate::pipeline_metrics::note_cardinality_drop(
                    "metric_aggregator_bytes",
                    used as usize,
                    budget as usize,
                );
                return false;
            }
            match self.used_bytes.compare_exchange_weak(
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

    /// Account memory that is already present and must not be dropped because
    /// the admission budget shrank after it was accepted. This bypasses the
    /// admission check deliberately: the debt blocks further admissions until
    /// the row is flushed or drained.
    fn force_charge_debt(&self, bytes: u64) {
        self.used_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    fn refund(&self, bytes: u64) {
        let _ = self
            .used_bytes
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(bytes))
            });
    }

    pub fn record(
        &self,
        key: AggregationKey,
        bytes_sent: i64,
        bytes_received: i64,
        is_attack: bool,
    ) {
        let (capacity, byte_budget) =
            crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        self.record_with_limits(
            key,
            bytes_sent,
            bytes_received,
            is_attack,
            capacity,
            byte_budget,
        );
    }

    #[cfg(test)]
    fn record_with_capacity(
        &self,
        key: AggregationKey,
        bytes_sent: i64,
        bytes_received: i64,
        is_attack: bool,
        capacity: usize,
    ) {
        self.record_with_limits(
            key,
            bytes_sent,
            bytes_received,
            is_attack,
            capacity,
            u64::MAX,
        );
    }

    fn record_with_limits(
        &self,
        key: AggregationKey,
        bytes_sent: i64,
        bytes_received: i64,
        is_attack: bool,
        capacity: usize,
        byte_budget: u64,
    ) {
        match self.data.entry(key) {
            Entry::Occupied(mut occupied) => {
                occupied
                    .get_mut()
                    .add(bytes_sent, bytes_received, is_attack);
            }
            Entry::Vacant(vacant) => {
                if self.entry_count.load(Ordering::Acquire) >= capacity {
                    crate::pipeline_metrics::note_cardinality_drop(
                        "metric_aggregator",
                        self.entry_count.load(Ordering::Relaxed),
                        capacity,
                    );
                    return;
                }
                let empty = AggregatedValue::default();
                let (live, temporary) = estimated_entry_bytes(vacant.key(), &empty);
                let base = live.saturating_add(temporary);
                if !self.try_charge(base, byte_budget) {
                    return;
                }
                let mut entry = AggregatedValue {
                    estimated_bytes: base,
                    ..Default::default()
                };
                entry.add(bytes_sent, bytes_received, is_attack);
                self.entry_count.fetch_add(1, Ordering::AcqRel);
                vacant.insert(entry);
            }
        }
    }

    /// Remove every row without refunding its charge. Ownership of the charge
    /// moves to the returned drain.
    pub fn drain(&self) -> AggregatorDrain {
        let keys: Vec<_> = self.data.iter().map(|entry| entry.key().clone()).collect();
        let mut rows = Vec::with_capacity(keys.len());
        let mut charged_bytes = 0u64;
        for key in keys {
            if let Some((key, value)) = self.data.remove(&key) {
                self.entry_count.fetch_sub(1, Ordering::AcqRel);
                charged_bytes = charged_bytes.saturating_add(value.estimated_bytes);
                rows.push((key, value));
            }
        }
        AggregatorDrain {
            rows,
            charged_bytes,
        }
    }

    /// Release the charge of a drain that has been durably handed off.
    pub fn release_charge(&self, bytes: u64) {
        self.refund(bytes);
    }

    /// Put a drain back without charging again: its charge was never refunded.
    /// Rows that were already accepted bypass the entry cap; a refilled queue
    /// must not make an accepted row disappear. The shared byte budget remains
    /// the bound, and a redundant charge for a merged occupied entry is
    /// refunded.
    pub fn restore_drained(&self, drain: AggregatorDrain) {
        let (capacity, byte_budget) =
            crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        self.restore_with_limits(drain.rows, capacity, byte_budget, true);
    }

    /// Restore rows whose charge was already released (for example a `flush`).
    /// They are accepted rows, so the entry cap is bypassed; the row is charged
    /// again where the budget allows.
    pub fn restore(&self, samples: Vec<(AggregationKey, AggregatedValue)>) {
        let (capacity, byte_budget) =
            crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        self.restore_with_limits(samples, capacity, byte_budget, false);
    }

    fn restore_with_limits(
        &self,
        samples: Vec<(AggregationKey, AggregatedValue)>,
        capacity: usize,
        byte_budget: u64,
        charge_held: bool,
    ) {
        for (key, value) in samples {
            match self.data.entry(key) {
                Entry::Occupied(mut occupied) => {
                    let entry = occupied.get_mut();
                    entry.count += value.count;
                    entry.count_attack += value.count_attack;
                    entry.bytes_sent += value.bytes_sent;
                    entry.bytes_received += value.bytes_received;
                    entry.attack_bytes += value.attack_bytes;
                    if charge_held {
                        // The counter merged into an entry that already owns a
                        // charge; the drained row's charge is now duplicate.
                        self.refund(value.estimated_bytes);
                    }
                }
                Entry::Vacant(vacant) => {
                    // Restore is not a new admission: rows were accepted
                    // before the drain and must not disappear because another
                    // key refilled the entry cap while they were queued.
                    let (live, temporary) = estimated_entry_bytes(vacant.key(), &value);
                    let needed = live.saturating_add(temporary);
                    let mut value = value;
                    if charge_held {
                        // Its charge was never refunded. Only estimate growth
                        // needs another reservation; if that fails, keep the
                        // original buffer and its original charge rather than
                        // dropping the accepted row.
                        let extra = needed.saturating_sub(value.estimated_bytes);
                        if extra > 0 && self.try_charge(extra, byte_budget) {
                            value.estimated_bytes = value.estimated_bytes.saturating_add(extra);
                        }
                    } else {
                        // The caller released the charge (flush). Charge it
                        // again when possible. If the budget shrank below the
                        // already-accepted rows, keep every row and still
                        // record its real footprint as debt, so later
                        // admissions stay blocked instead of the row becoming
                        // invisible to accounting.
                        if !self.try_charge(needed, byte_budget) {
                            self.force_charge_debt(needed);
                        }
                        value.estimated_bytes = needed;
                    }
                    let _ = capacity;
                    self.entry_count.fetch_add(1, Ordering::AcqRel);
                    vacant.insert(value);
                }
            }
        }
    }

    /// Detailed on-demand estimate. Walking entries holds shard read locks; the
    /// periodic status gauge uses `approximate_bytes` instead.
    pub fn memory_estimate(&self) -> MetricAggregatorMemoryEstimate {
        let mut estimate = MetricAggregatorMemoryEstimate::default();
        for entry in &self.data {
            let (live, temporary) = estimated_entry_bytes(entry.key(), entry.value());
            estimate.entries = estimate.entries.saturating_add(1);
            estimate.live_bytes = estimate.live_bytes.saturating_add(live);
            estimate.flush_temporary_bytes =
                estimate.flush_temporary_bytes.saturating_add(temporary);
        }
        estimate
    }

    pub fn approximate_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Acquire)
    }

    /// Drain and release the charge immediately. Used by memory audits and
    /// tests that do not hand rows to the snapshot store.
    pub fn flush(&self) -> Vec<(AggregationKey, AggregatedValue)> {
        let drain = self.drain();
        self.release_charge(drain.charged_bytes);
        drain.rows
    }
}

pub static HTTP_REQUEST_STAT_AGGREGATOR: Lazy<Arc<MetricAggregator>> =
    Lazy::new(|| Arc::new(MetricAggregator::new()));

#[cfg(test)]
mod tests {
    use super::{AggregationKey, MetricAggregator};
    use std::sync::Arc;

    fn test_key(server_id: i64, tag: &str) -> AggregationKey {
        AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id,
            window_start: 1_793_462_400,
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(tag),
            city_id: 0,
            provider: Arc::from("Unknown"),
            browser: Arc::from(""),
            os: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
            provider_id: 0,
            browser_version: Arc::from(""),
            os_version: Arc::from(""),
        }
    }

    #[test]
    fn metric_category_separates_otherwise_identical_rows() {
        let http_key = test_key(1, "");
        let tcp_key = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_TCP),
            ..http_key.clone()
        };
        assert_ne!(http_key, tcp_key);
    }

    #[test]
    fn event_minutes_separate_otherwise_identical_rows() {
        let aggregator = MetricAggregator::new();
        let first = test_key(1, "city");
        let second = AggregationKey {
            window_start: first.window_start + 60,
            ..first.clone()
        };
        aggregator.record_with_capacity(first.clone(), 10, 0, false, 4);
        aggregator.record_with_capacity(first, 20, 0, false, 4);
        aggregator.record_with_capacity(second, 30, 0, false, 4);
        let rows = aggregator.flush();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().map(|(_, value)| value.count).sum::<i64>(), 3);
    }

    #[test]
    fn draining_keeps_the_charge_until_released() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_capacity(test_key(1, "a"), 1, 0, false, 10);
        let admitted = aggregator.approximate_bytes();
        assert!(admitted > 0);
        let drain = aggregator.drain();
        assert_eq!(aggregator.approximate_bytes(), admitted);
        aggregator.release_charge(drain.charged_bytes);
        assert_eq!(aggregator.approximate_bytes(), 0);
    }

    #[test]
    fn restoring_a_drain_does_not_double_charge() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_capacity(test_key(1, "a"), 1, 0, false, 10);
        let admitted = aggregator.approximate_bytes();
        let drain = aggregator.drain();
        aggregator.restore_drained(drain);
        assert_eq!(aggregator.approximate_bytes(), admitted);
        assert_eq!(aggregator.data.len(), 1);
    }

    #[test]
    fn merging_a_drain_into_an_existing_row_refunds_the_duplicate_charge() {
        let aggregator = MetricAggregator::new();
        let key = test_key(1, "a");
        // Each record adds one request; loop so the merged count is 10 + 5.
        for _ in 0..10 {
            aggregator.record_with_capacity(key.clone(), 1, 0, false, 10);
        }
        let drain = aggregator.drain();
        for _ in 0..5 {
            aggregator.record_with_capacity(key.clone(), 1, 0, false, 10);
        }
        let charged_before_restore = aggregator.approximate_bytes();
        let restored_charge = drain.charged_bytes;
        aggregator.restore_drained(drain);
        assert_eq!(aggregator.data.get(&key).unwrap().count, 15);
        // The drain's held charge was already counted before the restore and
        // is refunded when the counter merges into the occupied row.
        assert_eq!(
            aggregator.approximate_bytes(),
            charged_before_restore.saturating_sub(restored_charge)
        );
        assert_eq!(aggregator.data.len(), 1);
    }

    #[test]
    fn restoring_an_accepted_drain_bypasses_a_refilled_entry_cap() {
        let aggregator = MetricAggregator::new();
        let first = test_key(1, "a");
        for _ in 0..10 {
            aggregator.record_with_capacity(first.clone(), 1, 0, false, 1);
        }
        let drain = aggregator.drain();
        // The other key refills the only admission slot while the first row is
        // queued for retry.
        aggregator.record_with_capacity(test_key(2, "b"), 20, 0, false, 1);
        let charged_before_restore = aggregator.approximate_bytes();
        let restored_charge = drain.charged_bytes;
        aggregator.restore_drained(drain);
        assert_eq!(aggregator.data.len(), 2);
        assert_eq!(aggregator.data.get(&first).unwrap().count, 10);
        // The restored row stays vacant (the other key owns a different
        // identity), so its drain charge was never refunded and is already
        // part of `charged_before_restore`; no second charge is added.
        assert_eq!(aggregator.approximate_bytes(), charged_before_restore);
        assert!(restored_charge > 0);
    }

    #[test]
    fn repeated_flush_and_restore_keeps_count_and_charge_constant() {
        let aggregator = MetricAggregator::new();
        let key = test_key(1, "a");
        aggregator.record_with_capacity(key.clone(), 1, 0, false, 10);
        let admitted = aggregator.approximate_bytes();
        for _ in 0..64 {
            let rows = aggregator.flush();
            assert_eq!(aggregator.approximate_bytes(), 0);
            aggregator.restore(rows);
            assert_eq!(aggregator.approximate_bytes(), admitted);
            assert_eq!(aggregator.data.get(&key).unwrap().count, 1);
        }
    }

    #[test]
    fn restoring_released_rows_records_debt_when_the_budget_shrank() {
        let aggregator = MetricAggregator::new();
        let key = test_key(1, "a");
        aggregator.record_with_capacity(key.clone(), 1, 0, false, 10);
        let rows = aggregator.flush();
        assert_eq!(aggregator.approximate_bytes(), 0);

        let (live, temporary) =
            super::estimated_entry_bytes(&key, &super::AggregatedValue::default());
        let needed = live + temporary;
        aggregator.restore_with_limits(rows, 10, needed - 1, false);

        // The accepted row survives a shrunken budget, and its real footprint
        // stays on the books as debt rather than becoming invisible (zero).
        assert_eq!(aggregator.data.len(), 1);
        assert_eq!(aggregator.approximate_bytes(), needed);
        aggregator.record_with_limits(test_key(2, "b"), 1, 0, false, 10, needed - 1);
        assert!(aggregator.data.get(&test_key(2, "b")).is_none());
    }

    #[test]
    fn memory_estimate_covers_keys_and_flush_buffers() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_capacity(test_key(1, "a"), 1, 0, false, 10);
        let baseline = aggregator.memory_estimate();
        assert_eq!(baseline.entries, 1);
        assert!(baseline.flush_temporary_bytes > 0);
        aggregator.record_with_capacity(test_key(1, "a"), 1, 0, false, 10);
        assert_eq!(aggregator.memory_estimate().entries, 1);
        let rows = aggregator.flush();
        assert_eq!(aggregator.memory_estimate().live_bytes, 0);
        assert_eq!(aggregator.approximate_bytes(), 0);
        aggregator.restore(rows);
        assert_eq!(aggregator.memory_estimate(), baseline);
        assert!(aggregator.approximate_bytes() > 0);
    }

    #[test]
    fn byte_budget_rejects_a_new_key_whole_entry() {
        let aggregator = MetricAggregator::new();
        let key = test_key(1, "a");
        let (live, temporary) =
            super::estimated_entry_bytes(&key, &super::AggregatedValue::default());
        let base = live + temporary;
        aggregator.record_with_limits(key.clone(), 1, 0, false, 10, base - 1);
        assert!(aggregator.data.is_empty());
        assert_eq!(aggregator.approximate_bytes(), 0);

        aggregator.record_with_limits(key.clone(), 1, 0, false, 10, base);
        assert_eq!(aggregator.data.len(), 1);
        assert_eq!(aggregator.approximate_bytes(), base);
        // An existing key still accumulates at the byte ceiling; no dimension
        // is separated from the count.
        aggregator.record_with_limits(key.clone(), 2, 0, false, 10, base);
        assert_eq!(aggregator.data.get(&key).unwrap().bytes_sent, 3);
        assert_eq!(aggregator.approximate_bytes(), base);
    }

    #[test]
    fn shrinking_budget_stops_growth_until_release() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_limits(test_key(1, "a"), 1, 0, false, 10, u64::MAX);
        let admitted = aggregator.approximate_bytes();
        let lower_budget = admitted - 1;
        aggregator.record_with_limits(test_key(2, "b"), 1, 0, false, 10, lower_budget);
        assert_eq!(aggregator.data.len(), 1);
        assert_eq!(aggregator.approximate_bytes(), admitted);
        let drain = aggregator.drain();
        aggregator.release_charge(drain.charged_bytes);
        aggregator.record_with_limits(test_key(2, "b"), 1, 0, false, 10, 1);
        assert!(aggregator.data.is_empty());
        assert_eq!(aggregator.approximate_bytes(), 0);
    }

    #[test]
    fn cardinality_cap_rejects_new_keys_but_keeps_existing() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_capacity(test_key(1, "a"), 10, 0, false, 2);
        aggregator.record_with_capacity(test_key(1, "b"), 10, 0, false, 2);
        aggregator.record_with_capacity(test_key(1, "a"), 5, 0, false, 2);
        let before = crate::pipeline_metrics::snapshot().metrics_cardinality_dropped;
        aggregator.record_with_capacity(test_key(1, "c"), 99, 0, false, 2);
        assert!(crate::pipeline_metrics::snapshot().metrics_cardinality_dropped > before);
        let rows = aggregator.flush();
        assert_eq!(rows.len(), 2);
        let total: i64 = rows.iter().map(|row| row.1.bytes_sent).sum();
        assert_eq!(total, 25);
    }
}
