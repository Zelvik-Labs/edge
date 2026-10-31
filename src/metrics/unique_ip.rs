//! Exact IP identities retained only until a short batch reaches the durable spool.
//! Longer-period uniqueness is computed by the backend's identity union.

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const CHUNK_ENTRIES: usize = 1024;
const ESTIMATED_ENTRY_BYTES: u64 = 128;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Observation {
    server_id: i64,
    window_start: i64,
    ip: IpAddr,
}

pub struct UniqueIpTracker {
    ips: DashMap<Observation, ()>,
    /// Includes rows removed from the set while a durable append is in flight.
    used: AtomicUsize,
    capacity: usize,
    refused: AtomicU64,
    enqueue_failures: AtomicU64,
}

impl UniqueIpTracker {
    fn new(capacity: usize) -> Self {
        Self {
            ips: DashMap::with_shard_amount(32),
            used: AtomicUsize::new(0),
            capacity,
            refused: AtomicU64::new(0),
            enqueue_failures: AtomicU64::new(0),
        }
    }

    pub fn record(&self, server_id: i64, started_at: i64, ip: IpAddr) -> bool {
        if server_id <= 0 {
            return false;
        }
        let observation = Observation {
            server_id,
            window_start: started_at.div_euclid(60) * 60,
            ip,
        };
        match self.ips.entry(observation) {
            Entry::Occupied(_) => true,
            Entry::Vacant(vacant) => {
                if self
                    .used
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                        (used < self.capacity).then_some(used + 1)
                    })
                    .is_err()
                {
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    crate::pipeline_metrics::note_cardinality_drop(
                        "unique_ip_microbatch",
                        self.used.load(Ordering::Acquire),
                        self.capacity,
                    );
                    return false;
                }
                vacant.insert(());
                true
            }
        }
    }

    pub fn status(&self) -> (usize, usize, u64, u64) {
        (
            self.used.load(Ordering::Acquire),
            self.capacity,
            self.refused.load(Ordering::Relaxed),
            self.enqueue_failures.load(Ordering::Relaxed),
        )
    }

    pub fn approximate_bytes(&self) -> u64 {
        (self.used.load(Ordering::Acquire) as u64).saturating_mul(ESTIMATED_ENTRY_BYTES)
    }

    fn drain_chunk(&self, limit: usize) -> Vec<Observation> {
        let keys: Vec<_> = self
            .ips
            .iter()
            .take(limit)
            .map(|entry| *entry.key())
            .collect();
        keys.into_iter()
            .filter_map(|key| self.ips.remove(&key).map(|(key, ())| key))
            .collect()
    }

    fn restore(&self, rows: impl IntoIterator<Item = Observation>) {
        for row in rows {
            if self.ips.insert(row, ()).is_some() {
                // A duplicate was accepted during the append. Its own charge
                // remains; only the redundant drained identity is released.
                self.used.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    async fn flush_to<S: UniqueIpSink>(&self, sink: &mut S) -> anyhow::Result<()> {
        // New traffic belongs to the next tick; never chase an unbounded input
        // stream in a single flush, or allocate a full-cardinality drain vector.
        let mut remaining = self.used.load(Ordering::Acquire);
        while remaining > 0 {
            let rows = self.drain_chunk(remaining.min(CHUNK_ENTRIES));
            if rows.is_empty() {
                break;
            }
            remaining = remaining.saturating_sub(rows.len());
            let mut groups: BTreeMap<(i64, i64), Vec<Observation>> = BTreeMap::new();
            for row in rows {
                groups
                    .entry((row.server_id, row.window_start))
                    .or_default()
                    .push(row);
            }
            let mut groups = groups.into_iter();
            while let Some(((server_id, window_start), rows)) = groups.next() {
                let request = crate::pb::UniqueIpObservations {
                    observations: rows
                        .iter()
                        .map(|row| crate::pb::UniqueIpObservation {
                            server_id: row.server_id,
                            window_start: row.window_start,
                            ip: row.ip.to_string(),
                        })
                        .collect(),
                };
                let result = sink.send(&request, window_start).await;
                drop(request);
                if let Err(error) = result {
                    self.enqueue_failures.fetch_add(1, Ordering::Relaxed);
                    self.restore(rows);
                    for (_, rows) in groups {
                        self.restore(rows);
                    }
                    tracing::warn!(server_id, window_start, %error,
                        "unique IP durable enqueue refused; unsent identities and charge retained");
                    return Err(error);
                }
                let count = rows.len();
                drop(rows);
                self.used.fetch_sub(count, Ordering::AcqRel);
            }
        }
        Ok(())
    }

    pub async fn flush(&self) -> anyhow::Result<()> {
        self.flush_to(&mut TelemetrySink).await
    }
}

trait UniqueIpSink {
    async fn send(
        &mut self,
        request: &crate::pb::UniqueIpObservations,
        window_start: i64,
    ) -> anyhow::Result<()>;
}

struct TelemetrySink;

impl UniqueIpSink for TelemetrySink {
    async fn send(
        &mut self,
        request: &crate::pb::UniqueIpObservations,
        window_start: i64,
    ) -> anyhow::Result<()> {
        crate::telemetry::enqueue("unique_ip", request, window_start, false).await
    }
}

pub static UNIQUE_IP_TRACKER: LazyLock<UniqueIpTracker> = LazyLock::new(|| {
    UniqueIpTracker::new(crate::memory_governor::MEMORY_GOVERNOR.metrics_distinct_ip_capacity())
});

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    struct DurableSink<'a> {
        tracker: &'a UniqueIpTracker,
        spool: crate::telemetry::Spool,
        rows: Vec<crate::pb::UniqueIpObservation>,
    }

    impl UniqueIpSink for DurableSink<'_> {
        async fn send(
            &mut self,
            request: &crate::pb::UniqueIpObservations,
            window_start: i64,
        ) -> anyhow::Result<()> {
            assert!(request.observations.len() <= CHUNK_ENTRIES);
            assert!(self.tracker.used.load(Ordering::Acquire) >= request.observations.len());
            self.spool.append(
                "unique_ip",
                request.encode_to_vec(),
                window_start,
                false,
                window_start + 60,
            )?;
            self.rows.extend(request.observations.iter().cloned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn repeated_new_ip_batches_release_all_state_after_durable_append() {
        let path = std::env::temp_dir().join(format!("zelvik-unique-{}", uuid::Uuid::new_v4()));
        let tracker = UniqueIpTracker::new(2048);
        let mut sink = DurableSink {
            tracker: &tracker,
            spool: crate::telemetry::Spool::open(&path, true, 4 * 1024 * 1024).unwrap(),
            rows: Vec::new(),
        };
        for batch in 0..4 {
            for index in 0..1100 {
                let ip = IpAddr::V6(std::net::Ipv6Addr::from((batch * 1100 + index + 1) as u128));
                assert!(tracker.record(7, 120 + batch * 60, ip));
            }
            tracker.flush_to(&mut sink).await.unwrap();
            assert_eq!(tracker.status().0, 0);
            assert!(tracker.ips.is_empty());
            assert_eq!(tracker.approximate_bytes(), 0);
        }
        assert_eq!(sink.rows.len(), 4400);
        let mut persisted = Vec::new();
        let mut offset = 0;
        while let Some(batch) = sink.spool.at(offset).unwrap() {
            assert_eq!(batch.kind, "unique_ip");
            let request =
                crate::pb::UniqueIpObservations::decode(batch.payload.as_slice()).unwrap();
            assert!(
                request
                    .observations
                    .iter()
                    .all(|row| row.window_start == batch.window_start)
            );
            persisted.extend(request.observations);
            offset += 1;
        }
        assert_eq!(persisted, sink.rows);
        for _ in 0..4 {
            tracker.flush_to(&mut sink).await.unwrap();
            assert_eq!(tracker.status().0, 0);
        }
        assert_eq!(sink.rows.len(), 4400, "no input creates no rows");
        drop(sink);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn ipv4_ipv6_duplicates_are_exact_per_server_and_event_minute() {
        let tracker = UniqueIpTracker::new(8);
        let v4 = "192.0.2.1".parse().unwrap();
        let v6 = "2001:db8::1".parse().unwrap();
        for ip in [v4, v6] {
            assert!(tracker.record(1, 120, ip));
            assert!(tracker.record(1, 179, ip));
            assert!(tracker.record(1, 180, ip));
            assert!(tracker.record(2, 120, ip));
        }
        assert_eq!(tracker.status().0, 6);
        let rows = tracker.drain_chunk(8);
        assert_eq!(rows.len(), 6);
        assert!(
            rows.iter()
                .all(|row| row.window_start == 120 || row.window_start == 180)
        );
        assert_eq!(rows.iter().filter(|row| row.ip == v4).count(), 3);
        tracker.restore(rows);
        assert_eq!(tracker.status().0, 6);
    }

    #[test]
    fn canonical_ip_strings_preserve_mapped_ipv6_identity() {
        let tracker = UniqueIpTracker::new(3);
        for text in [
            "192.0.2.1",
            "::ffff:192.0.2.1",
            "2001:0db8:0:0::1",
            "2001:db8::1",
        ] {
            assert!(tracker.record(1, 120, text.parse().unwrap()));
        }
        let mut ips: Vec<_> = tracker
            .drain_chunk(3)
            .iter()
            .map(|row| row.ip.to_string())
            .collect();
        ips.sort();
        assert_eq!(ips, vec!["192.0.2.1", "2001:db8::1", "::ffff:192.0.2.1"]);
    }

    struct RefusingSink<'a>(&'a UniqueIpTracker);

    impl UniqueIpSink for RefusingSink<'_> {
        async fn send(
            &mut self,
            _: &crate::pb::UniqueIpObservations,
            _: i64,
        ) -> anyhow::Result<()> {
            // Removing rows for enqueue must not free admission capacity.
            assert_eq!(self.0.status().0, 2);
            assert!(!self.0.record(3, 180, "192.0.2.3".parse().unwrap()));
            anyhow::bail!("durable spool refused")
        }
    }

    #[tokio::test]
    async fn enqueue_refusal_restores_all_rows_and_keeps_the_budget_charged() {
        let tracker = UniqueIpTracker::new(2);
        assert!(tracker.record(1, 120, "192.0.2.1".parse().unwrap()));
        assert!(tracker.record(2, 180, "2001:db8::1".parse().unwrap()));
        assert!(tracker.flush_to(&mut RefusingSink(&tracker)).await.is_err());
        assert_eq!(tracker.status(), (2, 2, 1, 1));
        assert_eq!(tracker.ips.len(), 2);
        assert!(tracker.record(1, 179, "192.0.2.1".parse().unwrap()));
        assert_eq!(tracker.status().0, 2);
    }

    struct PartialSink {
        delivered: Vec<crate::pb::UniqueIpObservation>,
        calls: usize,
    }

    impl UniqueIpSink for PartialSink {
        async fn send(
            &mut self,
            request: &crate::pb::UniqueIpObservations,
            _: i64,
        ) -> anyhow::Result<()> {
            self.calls += 1;
            if self.calls == 2 {
                anyhow::bail!("second durable append refused");
            }
            self.delivered.extend(request.observations.iter().cloned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn partial_enqueue_restores_only_unsent_identities_then_releases_on_retry() {
        let tracker = UniqueIpTracker::new(4);
        for server in 1..=3 {
            assert!(tracker.record(server, 120, "192.0.2.1".parse().unwrap()));
        }
        let mut sink = PartialSink {
            delivered: Vec::new(),
            calls: 0,
        };
        assert!(tracker.flush_to(&mut sink).await.is_err());
        assert_eq!(tracker.status().0, 2);
        assert_eq!(sink.delivered.len(), 1);
        tracker.flush_to(&mut sink).await.unwrap();
        assert_eq!(tracker.status().0, 0);
        assert!(tracker.ips.is_empty());
        sink.delivered.sort_by_key(|row| row.server_id);
        assert_eq!(
            sink.delivered
                .iter()
                .map(|row| row.server_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn request_start_hooks_capture_even_when_a_connection_already_recorded_an_ip() {
        let server_id = 8_830_077;
        let metrics = std::sync::Arc::new(crate::metrics::ServerMetrics::new());
        assert!(crate::metrics::record::request_start(
            server_id,
            "192.0.2.77",
            0,
            0,
            0,
            Some(&metrics),
            true,
        ));
        assert!(crate::metrics::record::request_start_without_active(
            server_id,
            "2001:db8::77",
            0,
            0,
            0,
            Some(&metrics),
            true,
        ));
        let keys: Vec<_> = UNIQUE_IP_TRACKER
            .ips
            .iter()
            .filter(|row| row.key().server_id == server_id)
            .map(|row| *row.key())
            .collect();
        assert_eq!(
            keys.len(),
            2,
            "both protocol hooks ignore the old connection-lifetime flag"
        );
        for key in keys {
            assert!(UNIQUE_IP_TRACKER.ips.remove(&key).is_some());
            UNIQUE_IP_TRACKER.used.fetch_sub(1, Ordering::AcqRel);
        }
        crate::metrics::record::request_end(server_id, 0, 0, false, false, false, Some(&metrics));
        assert_eq!(metrics.active_connections.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn restore_merges_concurrent_duplicate_without_double_charge() {
        let tracker = UniqueIpTracker::new(2);
        let ip = "192.0.2.1".parse().unwrap();
        assert!(tracker.record(1, 120, ip));
        let rows = tracker.drain_chunk(1);
        assert!(tracker.record(1, 120, ip));
        assert_eq!(tracker.status().0, 2);
        tracker.restore(rows);
        assert_eq!(tracker.status().0, 1);
        assert_eq!(tracker.ips.len(), 1);
    }
}
