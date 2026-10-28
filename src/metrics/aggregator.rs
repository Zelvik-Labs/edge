use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Clone)]
pub struct AggregationKey {
    pub category: Arc<str>,
    pub server_id: i64,
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
    pub request_attrs: Arc<BTreeMap<String, String>>,
}

impl PartialEq for AggregationKey {
    fn eq(&self, other: &Self) -> bool {
        self.category == other.category
            && self.server_id == other.server_id
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

impl AggregationKey {
    pub fn resolve_metric_key(&self, configured_key: &str) -> String {
        self.resolve_metric_key_with_attrs(configured_key, self.request_attrs.as_ref())
    }

    pub fn resolve_metric_key_with_attrs(
        &self,
        configured_key: &str,
        request_attrs: &BTreeMap<String, String>,
    ) -> String {
        crate::metrics::resolve_template_with(configured_key, |var_name| {
            let resolved =
                crate::metrics::resolve_metric_variable_from_map(request_attrs, var_name);
            if !resolved.is_empty() && resolved != format!("${{{var_name}}}") {
                return resolved;
            }

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
                _ => resolved,
            }
        })
    }
}

#[derive(Debug, Default, Clone)]
pub struct AggregatedRequestValue {
    pub count: i64,
    pub count_attack: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub attack_bytes: i64,
}

impl AggregatedRequestValue {
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

#[derive(Debug, Default, Clone)]
pub struct AggregatedValue {
    pub count: i64,
    pub count_attack: i64,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub attack_bytes: i64,
    pub request_samples: BTreeMap<Arc<BTreeMap<String, String>>, AggregatedRequestValue>,
    /// Bytes charged for this entry, including its flush/restore copy.
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

/// Estimate of live aggregator allocations and additional flush vectors.
/// Shared `Arc` allocations may be counted more than once; allocator slack and
/// container implementation details are not observable here. This is neither
/// RSS nor an enforced allocation budget.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MetricAggregatorMemoryEstimate {
    pub entries: usize,
    pub request_samples: usize,
    pub live_bytes: u64,
    pub flush_temporary_bytes: u64,
}

fn estimated_attrs_bytes(attrs: &BTreeMap<String, String>) -> u64 {
    // Include the Arc allocation, map header and a per-row allowance for
    // B-tree nodes; String capacities account for dynamically supplied data.
    let base =
        (std::mem::size_of::<BTreeMap<String, String>>() + 2 * std::mem::size_of::<usize>()) as u64;
    attrs.iter().fold(base, |total, (key, value)| {
        total
            .saturating_add(128)
            .saturating_add(key.capacity() as u64)
            .saturating_add(value.capacity() as u64)
    })
}

fn estimated_sample_bytes(
    attrs: &Arc<BTreeMap<String, String>>,
    key_attrs: &Arc<BTreeMap<String, String>>,
) -> u64 {
    let mut bytes = 128u64;
    if !Arc::ptr_eq(attrs, key_attrs) {
        bytes = bytes.saturating_add(estimated_attrs_bytes(attrs.as_ref()));
    }
    bytes
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
    // Account for the key/value, a DashMap bucket allowance, and the Arc
    // headers and string data. Request attributes on the key are included
    // even when request samples are not preserved.
    let mut live = (std::mem::size_of::<(AggregationKey, AggregatedValue)>() + 32) as u64;
    for dimension in dimensions {
        live =
            live.saturating_add(dimension.len() as u64 + 2 * std::mem::size_of::<usize>() as u64);
    }
    live = live.saturating_add(estimated_attrs_bytes(&key.request_attrs));
    for attrs in value.request_samples.keys() {
        live = live.saturating_add(128);
        if !Arc::ptr_eq(attrs, &key.request_attrs) {
            live = live.saturating_add(estimated_attrs_bytes(attrs));
        }
    }
    let temporary = (std::mem::size_of::<AggregationKey>()
        + 2 * std::mem::size_of::<(AggregationKey, AggregatedValue)>()) as u64;
    (live, temporary)
}

pub struct MetricAggregator {
    pub data: DashMap<AggregationKey, AggregatedValue>,
    preserve_request_samples: bool,
    used_bytes: AtomicU64,
    entry_count: AtomicUsize,
}

impl Default for MetricAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricAggregator {
    pub fn new() -> Self {
        Self::with_request_samples(false)
    }

    pub fn with_request_samples(preserve_request_samples: bool) -> Self {
        Self {
            data: DashMap::with_shard_amount(64),
            preserve_request_samples,
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
        let request_attrs = self
            .preserve_request_samples
            .then(|| Arc::clone(&key.request_attrs));
        match self.data.entry(key) {
            Entry::Occupied(mut occupied) => {
                let sample = request_attrs.map(|attrs| {
                    let cost = estimated_sample_bytes(&attrs, &occupied.key().request_attrs);
                    (attrs, cost)
                });
                let entry = occupied.get_mut();
                entry.add(bytes_sent, bytes_received, is_attack);
                if let Some((attrs, cost)) = sample {
                    if let Some(sample) = entry.request_samples.get_mut(&attrs) {
                        sample.add(bytes_sent, bytes_received, is_attack);
                    } else if entry.request_samples.len() >= capacity {
                        crate::pipeline_metrics::note_cardinality_drop(
                            "metric_request_samples",
                            entry.request_samples.len(),
                            capacity,
                        );
                    } else if self.try_charge(cost, byte_budget) {
                        let mut sample = AggregatedRequestValue::default();
                        sample.add(bytes_sent, bytes_received, is_attack);
                        entry.request_samples.insert(attrs, sample);
                        entry.estimated_bytes = entry.estimated_bytes.saturating_add(cost);
                    }
                }
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
                if let Some(attrs) = request_attrs {
                    let cost = estimated_sample_bytes(&attrs, &vacant.key().request_attrs);
                    if entry.request_samples.len() >= capacity {
                        crate::pipeline_metrics::note_cardinality_drop(
                            "metric_request_samples",
                            0,
                            capacity,
                        );
                    } else if self.try_charge(cost, byte_budget) {
                        let mut sample = AggregatedRequestValue::default();
                        sample.add(bytes_sent, bytes_received, is_attack);
                        entry.request_samples.insert(attrs, sample);
                        entry.estimated_bytes = entry.estimated_bytes.saturating_add(cost);
                    }
                }
                self.entry_count.fetch_add(1, Ordering::AcqRel);
                vacant.insert(entry);
            }
        }
    }

    pub fn flush(&self) -> Vec<(AggregationKey, AggregatedValue)> {
        let mut samples = Vec::new();
        let keys: Vec<_> = self.data.iter().map(|e| e.key().clone()).collect();
        for k in keys {
            if let Some((key, val)) = self.data.remove(&k) {
                self.entry_count.fetch_sub(1, Ordering::AcqRel);
                self.refund(val.estimated_bytes);
                samples.push((key, val));
            }
        }
        samples
    }

    pub fn restore(&self, samples: Vec<(AggregationKey, AggregatedValue)>) {
        let (capacity, byte_budget) =
            crate::memory_governor::MEMORY_GOVERNOR.metrics_aggregator_limits();
        self.restore_with_limits(samples, capacity, byte_budget);
    }

    #[cfg(test)]
    fn restore_with_capacity(
        &self,
        samples: Vec<(AggregationKey, AggregatedValue)>,
        capacity: usize,
    ) {
        self.restore_with_limits(samples, capacity, u64::MAX);
    }

    fn restore_with_limits(
        &self,
        samples: Vec<(AggregationKey, AggregatedValue)>,
        capacity: usize,
        byte_budget: u64,
    ) {
        for (key, value) in samples {
            match self.data.entry(key) {
                Entry::Occupied(mut occupied) => {
                    let key_attrs = Arc::clone(&occupied.key().request_attrs);
                    let entry = occupied.get_mut();
                    entry.count += value.count;
                    entry.count_attack += value.count_attack;
                    entry.bytes_sent += value.bytes_sent;
                    entry.bytes_received += value.bytes_received;
                    entry.attack_bytes += value.attack_bytes;
                    for (attrs, request_value) in value.request_samples {
                        if let Some(target) = entry.request_samples.get_mut(&attrs) {
                            target.count += request_value.count;
                            target.count_attack += request_value.count_attack;
                            target.bytes_sent += request_value.bytes_sent;
                            target.bytes_received += request_value.bytes_received;
                            target.attack_bytes += request_value.attack_bytes;
                            continue;
                        }
                        if entry.request_samples.len() >= capacity {
                            crate::pipeline_metrics::note_cardinality_drop(
                                "metric_request_samples",
                                entry.request_samples.len(),
                                capacity,
                            );
                            continue;
                        }
                        let cost = estimated_sample_bytes(&attrs, &key_attrs);
                        if !self.try_charge(cost, byte_budget) {
                            continue;
                        }
                        entry.request_samples.insert(attrs, request_value);
                        entry.estimated_bytes = entry.estimated_bytes.saturating_add(cost);
                    }
                }
                Entry::Vacant(vacant) => {
                    if self.entry_count.load(Ordering::Acquire) >= capacity {
                        crate::pipeline_metrics::note_cardinality_drop(
                            "metric_aggregator",
                            self.entry_count.load(Ordering::Relaxed),
                            capacity,
                        );
                        continue;
                    }
                    let (live, temporary) = estimated_entry_bytes(vacant.key(), &value);
                    let cost = value.estimated_bytes.max(live.saturating_add(temporary));
                    if !self.try_charge(cost, byte_budget) {
                        continue;
                    }
                    let mut value = value;
                    value.estimated_bytes = cost;
                    self.entry_count.fetch_add(1, Ordering::AcqRel);
                    vacant.insert(value);
                }
            }
        }
    }

    /// Detailed on-demand estimate. Walking nested attributes holds shard read
    /// locks; the periodic status gauge uses `approximate_bytes` instead.
    pub fn memory_estimate(&self) -> MetricAggregatorMemoryEstimate {
        let mut estimate = MetricAggregatorMemoryEstimate::default();
        for entry in &self.data {
            let (live, temporary) = estimated_entry_bytes(entry.key(), entry.value());
            estimate.entries = estimate.entries.saturating_add(1);
            estimate.request_samples = estimate
                .request_samples
                .saturating_add(entry.value().request_samples.len());
            estimate.live_bytes = estimate.live_bytes.saturating_add(live);
            estimate.flush_temporary_bytes =
                estimate.flush_temporary_bytes.saturating_add(temporary);
        }
        estimate
    }

    pub fn approximate_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::Acquire)
    }
}

pub static METRIC_STAT_AGGREGATOR: Lazy<Arc<MetricAggregator>> =
    Lazy::new(|| Arc::new(MetricAggregator::with_request_samples(true)));

pub static HTTP_REQUEST_STAT_AGGREGATOR: Lazy<Arc<MetricAggregator>> =
    Lazy::new(|| Arc::new(MetricAggregator::new()));

#[cfg(test)]
mod tests {
    use super::{AggregationKey, MetricAggregator};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    #[test]
    fn metric_key_resolves_request_and_dimension_templates() {
        let mut attrs = BTreeMap::new();
        attrs.insert("host".to_string(), "cdn.example.com".to_string());
        attrs.insert("status".to_string(), "403".to_string());

        let key = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: Arc::from("China"),
            country_id: 1,
            province: Arc::from("Guangdong"),
            province_id: 2,
            city: Arc::from("Shenzhen"),
            city_id: 3,
            provider: Arc::from("CMCC"),
            browser: Arc::from("Chrome"),
            os: Arc::from("macOS"),
            waf_group_id: 9,
            waf_action: Arc::from("block"),
            provider_id: 258,
            browser_version: Arc::from("126"),
            os_version: Arc::from("14"),
            request_attrs: Arc::new(attrs),
        };

        assert_eq!(
            key.resolve_metric_key("${host}-${status}-${geo.city.name}-${browser.version}"),
            "cdn.example.com-403-Shenzhen-126"
        );
        assert_eq!(key.resolve_metric_key("${unknownVar}"), "${unknownVar}");
    }

    #[test]
    fn metric_category_separates_otherwise_identical_rows() {
        let request_attrs = Arc::new(BTreeMap::new());
        let http_key = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(""),
            city_id: 0,
            provider: Arc::from("Unknown"),
            browser: Arc::from(""),
            os: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
            provider_id: 0,
            browser_version: Arc::from(""),
            os_version: Arc::from(""),
            request_attrs: Arc::clone(&request_attrs),
        };
        let tcp_key = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_TCP),
            request_attrs,
            ..http_key.clone()
        };

        assert_ne!(http_key, tcp_key);
    }

    #[test]
    fn request_attrs_do_not_split_stable_aggregation_key() {
        let mut attrs_a = BTreeMap::new();
        attrs_a.insert("remoteAddr".to_string(), "203.0.113.1".to_string());
        attrs_a.insert("requestId".to_string(), "a".to_string());
        let mut attrs_b = BTreeMap::new();
        attrs_b.insert("remoteAddr".to_string(), "203.0.113.1".to_string());
        attrs_b.insert("requestId".to_string(), "b".to_string());

        let base = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(""),
            city_id: 0,
            provider: Arc::from("Unknown"),
            browser: Arc::from(""),
            os: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
            provider_id: 0,
            browser_version: Arc::from(""),
            os_version: Arc::from(""),
            request_attrs: Arc::new(attrs_a),
        };
        let other = AggregationKey {
            request_attrs: Arc::new(attrs_b),
            ..base.clone()
        };

        assert_eq!(base, other);
    }

    #[test]
    fn metric_stat_aggregator_preserves_request_samples_for_later_key_grouping() {
        let aggregator = crate::metrics::aggregator::MetricAggregator::with_request_samples(true);
        let mut attrs_a = BTreeMap::new();
        attrs_a.insert("remoteAddr".to_string(), "203.0.113.1".to_string());
        attrs_a.insert("requestId".to_string(), "a".to_string());
        let mut attrs_b = BTreeMap::new();
        attrs_b.insert("remoteAddr".to_string(), "203.0.113.1".to_string());
        attrs_b.insert("requestId".to_string(), "b".to_string());

        let base = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(""),
            city_id: 0,
            provider: Arc::from("Unknown"),
            browser: Arc::from(""),
            os: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
            provider_id: 0,
            browser_version: Arc::from(""),
            os_version: Arc::from(""),
            request_attrs: Arc::new(attrs_a),
        };
        let other = AggregationKey {
            request_attrs: Arc::new(attrs_b),
            ..base.clone()
        };

        aggregator.record(base, 10, 1, false);
        aggregator.record(other, 20, 2, false);

        let rows = aggregator.flush();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.count, 2);
        assert_eq!(rows[0].1.bytes_sent, 30);
        assert_eq!(rows[0].1.request_samples.len(), 2);
    }

    #[test]
    fn flushed_samples_can_be_restored_without_changing_totals() {
        let aggregator = MetricAggregator::with_request_samples(true);
        let key = AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id: 1,
            country: Arc::from(""),
            country_id: 0,
            province: Arc::from(""),
            province_id: 0,
            city: Arc::from(""),
            city_id: 0,
            provider: Arc::from("Unknown"),
            browser: Arc::from(""),
            os: Arc::from(""),
            waf_group_id: 0,
            waf_action: Arc::from(""),
            provider_id: 0,
            browser_version: Arc::from(""),
            os_version: Arc::from(""),
            request_attrs: Arc::new(BTreeMap::new()),
        };
        aggregator.record(key.clone(), 11, 7, true);
        let samples = aggregator.flush();
        assert_eq!(samples.len(), 1);
        assert!(aggregator.data.is_empty());
        aggregator.restore(samples);
        let restored = aggregator.flush();
        assert_eq!(restored[0].0, key);
        assert_eq!(restored[0].1.count, 1);
        assert_eq!(restored[0].1.bytes_sent, 11);
        assert_eq!(restored[0].1.bytes_received, 7);
        assert_eq!(restored[0].1.count_attack, 1);
    }

    fn test_key(server_id: i64, tag: &str) -> AggregationKey {
        AggregationKey {
            category: Arc::from(crate::metrics::METRIC_CATEGORY_HTTP),
            server_id,
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
            request_attrs: Arc::new(BTreeMap::new()),
        }
    }

    #[test]
    fn memory_estimate_includes_dynamic_attributes_and_flush_buffers() {
        let aggregator = MetricAggregator::with_request_samples(true);
        let mut first = test_key(1, "a");
        first.request_attrs = Arc::new(BTreeMap::from([("host".into(), "a".into())]));
        aggregator.record_with_capacity(first.clone(), 1, 0, false, 10);
        let baseline = aggregator.memory_estimate();
        assert_eq!(baseline.entries, 1);
        assert_eq!(baseline.request_samples, 1);
        assert!(baseline.flush_temporary_bytes > 0);

        let mut second = first;
        second.request_attrs = Arc::new(BTreeMap::from([("host".into(), "x".repeat(4096))]));
        aggregator.record_with_capacity(second, 1, 0, false, 10);
        let after = aggregator.memory_estimate();
        assert_eq!(after.entries, 1);
        assert_eq!(after.request_samples, 2);
        assert!(after.live_bytes >= baseline.live_bytes + 4096);
        assert_eq!(after.flush_temporary_bytes, baseline.flush_temporary_bytes);

        let rows = aggregator.flush();
        assert_eq!(aggregator.memory_estimate().live_bytes, 0);
        aggregator.restore(rows);
        assert_eq!(aggregator.memory_estimate(), after);
    }

    #[test]
    fn memory_estimate_deduplicates_key_and_sample_attribute_arc() {
        let mut key = test_key(1, "a");
        key.request_attrs = Arc::new(BTreeMap::from([("host".into(), "a".repeat(256))]));
        let mut value = super::AggregatedValue::default();
        value
            .request_samples
            .insert(Arc::clone(&key.request_attrs), Default::default());
        let (shared, _) = super::estimated_entry_bytes(&key, &value);
        value.request_samples.clear();
        value
            .request_samples
            .insert(Arc::new((*key.request_attrs).clone()), Default::default());
        let (separate, _) = super::estimated_entry_bytes(&key, &value);
        assert_eq!(
            separate - shared,
            super::estimated_attrs_bytes(&key.request_attrs)
        );
    }

    #[test]
    fn memory_estimate_includes_key_attributes_without_samples() {
        let aggregator = MetricAggregator::new();
        let mut key = test_key(1, "a");
        key.request_attrs = Arc::new(BTreeMap::from([("uri".into(), "x".repeat(4096))]));
        aggregator.record_with_capacity(key, 1, 0, false, 10);
        let single = aggregator.memory_estimate();
        assert_eq!(single.entries, 1);
        assert_eq!(single.request_samples, 0);
        assert!(single.live_bytes >= 4096);
        aggregator.record_with_capacity(test_key(2, "b"), 1, 0, false, 10);
        let two = aggregator.memory_estimate();
        assert_eq!(two.entries, 2);
        assert_eq!(two.flush_temporary_bytes, single.flush_temporary_bytes * 2);
    }

    #[test]
    fn byte_budget_rejects_key_and_sample_before_allocation() {
        let aggregator = MetricAggregator::with_request_samples(true);
        let mut key = test_key(1, "a");
        let attrs_a = Arc::new(BTreeMap::from([("requestId".into(), "a".into())]));
        key.request_attrs = Arc::clone(&attrs_a);
        let (live, temporary) =
            super::estimated_entry_bytes(&key, &super::AggregatedValue::default());
        let base = live + temporary;
        let sample_a = super::estimated_sample_bytes(&attrs_a, &key.request_attrs);

        aggregator.record_with_limits(key.clone(), 1, 0, false, 10, base - 1);
        assert!(aggregator.data.is_empty());
        assert_eq!(aggregator.approximate_bytes(), 0);

        aggregator.record_with_limits(key.clone(), 1, 0, false, 10, base + sample_a);
        assert_eq!(aggregator.data.len(), 1);
        assert_eq!(aggregator.approximate_bytes(), base + sample_a);
        assert_eq!(aggregator.data.get(&key).unwrap().request_samples.len(), 1);

        let attrs_b = Arc::new(BTreeMap::from([("requestId".into(), "b".into())]));
        let sample_b = super::estimated_sample_bytes(&attrs_b, &key.request_attrs);
        let key_b = AggregationKey {
            request_attrs: attrs_b,
            ..key.clone()
        };
        let used = aggregator.approximate_bytes();
        aggregator.record_with_limits(key_b, 1, 0, false, 10, used + sample_b - 1);
        let entry = aggregator.data.get(&key).unwrap();
        assert_eq!(entry.count, 2, "aggregate totals still accumulate");
        assert_eq!(
            entry.request_samples.len(),
            1,
            "a rejected sample must not be allocated"
        );
        drop(entry);
        assert_eq!(aggregator.approximate_bytes(), used);

        aggregator.flush();
        assert_eq!(aggregator.approximate_bytes(), 0);
    }

    #[test]
    fn restore_charges_the_same_budget_and_rejects_oversize_samples() {
        let source = MetricAggregator::with_request_samples(true);
        let mut key = test_key(1, "a");
        let attrs = Arc::new(BTreeMap::from([("requestId".into(), "a".into())]));
        key.request_attrs = Arc::clone(&attrs);
        source.record_with_capacity(key.clone(), 1, 0, false, 10);
        let samples = source.flush();

        let target = MetricAggregator::with_request_samples(true);
        target.restore_with_limits(samples.clone(), 10, 1);
        assert!(target.data.is_empty());
        assert_eq!(target.approximate_bytes(), 0);

        target.restore_with_limits(samples, 10, u64::MAX);
        assert_eq!(target.data.len(), 1);
        assert!(target.approximate_bytes() > 0);
        target.flush();
        assert_eq!(target.approximate_bytes(), 0);
    }

    #[test]
    fn cardinality_cap_rejects_new_keys_but_keeps_existing() {
        let aggregator = MetricAggregator::new();
        aggregator.record_with_capacity(test_key(1, "a"), 10, 0, false, 2);
        aggregator.record_with_capacity(test_key(1, "b"), 10, 0, false, 2);
        // Existing keys still accumulate at the cap.
        aggregator.record_with_capacity(test_key(1, "a"), 5, 0, false, 2);
        let before = crate::pipeline_metrics::snapshot().metrics_cardinality_dropped;
        aggregator.record_with_capacity(test_key(1, "c"), 99, 0, false, 2);
        assert!(crate::pipeline_metrics::snapshot().metrics_cardinality_dropped > before);

        let rows = aggregator.flush();
        assert_eq!(rows.len(), 2);
        let total: i64 = rows.iter().map(|r| r.1.bytes_sent).sum();
        assert_eq!(total, 25);

        // Restoring flushed rows beyond the cap merges into survivors only.
        let surplus = MetricAggregator::new();
        surplus.record_with_capacity(test_key(1, "x"), 1, 0, false, 2);
        surplus.record_with_capacity(test_key(1, "y"), 1, 0, false, 2);
        surplus.restore_with_capacity(
            vec![
                (test_key(1, "z"), Default::default()),
                (test_key(1, "w"), Default::default()),
            ],
            2,
        );
        assert_eq!(surplus.data.len(), 2);
    }
}
