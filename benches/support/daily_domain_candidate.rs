use dashmap::DashMap;
use std::sync::Arc;

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
struct DomainKey {
    server_id: i64,
    created_at: i64,
    domain: Arc<str>,
}

#[derive(Debug, Default, Clone)]
pub struct DomainStatValue {
    pub bytes: i64,
    pub cached_bytes: i64,
    pub count_requests: i64,
    pub count_cached_requests: i64,
    pub count_attack_requests: i64,
    pub attack_bytes: i64,
}

pub struct DailyDomainTracker {
    domains: DashMap<DomainKey, DomainStatValue>,
}

impl Default for DailyDomainTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DailyDomainTracker {
    pub fn new() -> Self {
        Self {
            domains: DashMap::new(),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "domain counters share one aggregation key"
    )]
    pub fn record(
        &self,
        server_id: i64,
        created_at: i64,
        domain: &str,
        bytes: i64,
        cached_bytes: i64,
        count_requests: i64,
        count_cached_requests: i64,
        count_attack_requests: i64,
        attack_bytes: i64,
    ) {
        if server_id <= 0 || domain.is_empty() || domain.len() >= 128 {
            return;
        }
        self.record_shared(
            server_id,
            created_at,
            Arc::from(domain),
            bytes,
            cached_bytes,
            count_requests,
            count_cached_requests,
            count_attack_requests,
            attack_bytes,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "domain counters share one aggregation key"
    )]
    pub fn record_shared(
        &self,
        server_id: i64,
        created_at: i64,
        domain: Arc<str>,
        bytes: i64,
        cached_bytes: i64,
        count_requests: i64,
        count_cached_requests: i64,
        count_attack_requests: i64,
        attack_bytes: i64,
    ) {
        self.record_with_capacity(
            server_id,
            created_at,
            domain,
            bytes,
            cached_bytes,
            count_requests,
            count_cached_requests,
            count_attack_requests,
            attack_bytes,
            || crate::memory_governor::MEMORY_GOVERNOR.metrics_cardinality_capacity(),
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "domain counters share one aggregation key"
    )]
    fn record_with_capacity(
        &self,
        server_id: i64,
        created_at: i64,
        domain: Arc<str>,
        bytes: i64,
        cached_bytes: i64,
        count_requests: i64,
        count_cached_requests: i64,
        count_attack_requests: i64,
        attack_bytes: i64,
        capacity: impl FnOnce() -> usize,
    ) {
        if server_id <= 0 || domain.is_empty() || domain.len() >= 128 {
            return;
        }

        let key = DomainKey {
            server_id,
            created_at,
            domain,
        };
        if let Some(mut entry) = self.domains.get_mut(&key) {
            entry.bytes += bytes;
            entry.cached_bytes += cached_bytes;
            entry.count_requests += count_requests;
            entry.count_cached_requests += count_cached_requests;
            entry.count_attack_requests += count_attack_requests;
            entry.attack_bytes += attack_bytes;
            return;
        }
        let capacity = capacity();
        if self.domains.len() >= capacity {
            crate::pipeline_metrics::note_cardinality_drop(
                "daily_domain_tracker",
                self.domains.len(),
                capacity,
            );
            return;
        }
        let mut entry = self.domains.entry(key).or_default();
        entry.bytes += bytes;
        entry.cached_bytes += cached_bytes;
        entry.count_requests += count_requests;
        entry.count_cached_requests += count_cached_requests;
        entry.count_attack_requests += count_attack_requests;
        entry.attack_bytes += attack_bytes;
    }

    /// Rough heap estimate: a shared domain (<=127 bytes), six counters
    /// and map overhead. Keep the existing observation scale.
    pub fn approximate_bytes(&self) -> u64 {
        (self.domains.len() as u64).saturating_mul(192)
    }

    pub fn flush_older_than(
        &self,
        current_created_at: i64,
    ) -> Vec<(i64, i64, String, DomainStatValue)> {
        let keys: Vec<_> = self
            .domains
            .iter()
            .filter(|entry| entry.key().created_at < current_created_at)
            .map(|entry| entry.key().clone())
            .collect();
        let mut rows = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((key, value)) = self.domains.remove(&key) {
                rows.push((key.server_id, key.created_at, key.domain.to_string(), value));
            }
        }
        rows
    }
}
