use dashmap::DashMap;
use std::sync::Arc;
use std::sync::LazyLock as Lazy;

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
pub static DAILY_DOMAIN_TRACKER: Lazy<Arc<DailyDomainTracker>> =
    Lazy::new(|| Arc::new(DailyDomainTracker::new()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_domain_approximate_bytes_tracks_live_entries() {
        let tracker = DailyDomainTracker::new();
        assert_eq!(tracker.approximate_bytes(), 0);
        tracker.record(1, 100, "example.com", 10, 5, 1, 1, 0, 0);
        tracker.record(1, 100, "cdn.example.com", 20, 0, 1, 0, 0, 0);
        assert_eq!(tracker.approximate_bytes(), 2 * 192);
    }

    #[test]
    fn shared_domain_keeps_ownership_and_all_counters_through_flush() {
        let tracker = DailyDomainTracker::new();
        let domain: Arc<str> = Arc::from("shared.example");
        tracker.record_shared(1, 100, Arc::clone(&domain), 13, 5, 2, 1, 1, 3);
        {
            let entry = tracker.domains.iter().next().unwrap();
            assert!(Arc::ptr_eq(&entry.key().domain, &domain));
        }
        // The compatible borrowed entry point must find the same key.
        tracker.record(1, 100, &domain, 7, 2, 3, 1, 2, 4);
        assert!(tracker.flush_older_than(100).is_empty());
        let rows = tracker.flush_older_than(101);
        assert_eq!(rows.len(), 1);
        let (server, day, name, value) = &rows[0];
        assert_eq!((*server, *day, name.as_str()), (1, 100, "shared.example"));
        assert_eq!(
            [
                value.bytes,
                value.cached_bytes,
                value.count_requests,
                value.count_cached_requests,
                value.count_attack_requests,
                value.attack_bytes
            ],
            [20, 7, 5, 2, 3, 7]
        );
        assert_eq!(tracker.approximate_bytes(), 0);
    }

    #[test]
    fn existing_and_invalid_daily_keys_do_not_consult_capacity() {
        let tracker = DailyDomainTracker::new();
        tracker.record_with_capacity(1, 100, Arc::from("a.com"), 10, 0, 1, 0, 0, 0, || 1);
        tracker.record_with_capacity(1, 100, Arc::from("a.com"), 5, 0, 1, 0, 0, 0, || {
            panic!("an existing key remains admissible under any current cap")
        });
        tracker.record_with_capacity(
            0,
            100,
            Arc::from("invalid.example"),
            1,
            0,
            1,
            0,
            0,
            0,
            || panic!("invalid keys do not require a capacity snapshot"),
        );
        tracker.record_with_capacity(1, 100, Arc::from("b.com"), 1, 0, 1, 0, 0, 0, || 0);
        let rows = tracker.flush_older_than(101);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "a.com");
        assert_eq!(rows[0].3.bytes, 15);
        assert_eq!(rows[0].3.count_requests, 2);
    }

    #[test]
    fn daily_domain_cardinality_cap_keeps_existing_keys() {
        let tracker = DailyDomainTracker::new();
        tracker.record_with_capacity(1, 100, Arc::from("a.com"), 10, 0, 1, 0, 0, 0, || 2);
        tracker.record_with_capacity(1, 100, Arc::from("b.com"), 10, 0, 1, 0, 0, 0, || 2);
        // Existing key accumulates at the cap.
        tracker.record_with_capacity(1, 100, Arc::from("a.com"), 5, 0, 1, 0, 0, 0, || 2);
        let before = crate::pipeline_metrics::snapshot().metrics_cardinality_dropped;
        tracker.record_with_capacity(1, 100, Arc::from("c.com"), 10, 0, 1, 0, 0, 0, || 2);
        assert!(crate::pipeline_metrics::snapshot().metrics_cardinality_dropped > before);
        assert_eq!(tracker.domains.len(), 2);
        assert_eq!(
            tracker
                .domains
                .get(&DomainKey {
                    server_id: 1,
                    created_at: 100,
                    domain: "a.com".into(),
                })
                .unwrap()
                .bytes,
            15
        );
        // Flushing older days still releases capped state.
        let rows = tracker.flush_older_than(200);
        assert_eq!(rows.len(), 2);
        assert_eq!(tracker.domains.len(), 0);
    }
}
