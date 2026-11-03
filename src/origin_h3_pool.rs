use dashmap::{DashMap, mapref::entry::Entry};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::origin_h3::{OriginH3Connection, OriginH3PoolKey};

const H3_POOL_IDLE: Duration = Duration::from_secs(120);
const H3_POOL_MAX_ENTRIES: usize = 4_096;
const SWEEP_INTERVAL_MS: u64 = 1_000;

struct SweepSchedule {
    epoch: Instant,
    next_ms: AtomicU64,
}

impl SweepSchedule {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            next_ms: AtomicU64::new(0),
        }
    }

    fn claim(&self, now: Instant) -> bool {
        let now_ms = u64::try_from(now.saturating_duration_since(self.epoch).as_millis())
            .unwrap_or(u64::MAX);
        let next = self.next_ms.load(Ordering::Relaxed);
        now_ms >= next
            && self
                .next_ms
                .compare_exchange(
                    next,
                    now_ms.saturating_add(SWEEP_INTERVAL_MS),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
    }
}

struct PooledH3Connection {
    context: Arc<OriginH3Connection>,
    last_used: Instant,
    _slot: OwnedSemaphorePermit,
}

struct H3ConnectionPool {
    entries: DashMap<OriginH3PoolKey, PooledH3Connection>,
    // An entry owns its idle-slot permit. Removal, expiry and replacement
    // cannot leak quota or overshoot it during concurrent releases.
    slots: Arc<Semaphore>,
    sweep: SweepSchedule,
}

impl H3ConnectionPool {
    fn new() -> Self {
        Self {
            entries: DashMap::new(),
            slots: Arc::new(Semaphore::new(H3_POOL_MAX_ENTRIES)),
            sweep: SweepSchedule::new(),
        }
    }

    fn take(&self, key: &OriginH3PoolKey) -> Option<Arc<OriginH3Connection>> {
        let now = Instant::now();
        self.maybe_sweep_at(now);
        let (_, pooled) = self.entries.remove(key)?;
        // A deferred global scan never extends this key's idle TTL.
        (!idle_expired(pooled.last_used, now) && pooled.context.is_open()).then_some(pooled.context)
    }

    fn offer(&self, key: OriginH3PoolKey, context: Arc<OriginH3Connection>) {
        if !context.is_open() {
            return;
        }
        self.maybe_sweep_at(Instant::now());
        if let Err((key, context)) = self.insert_or_replace(key, context) {
            // No shard guard is held here. At capacity, reclaim expired or
            // closed entries immediately before refusing a healthy connection.
            self.sweep_at(Instant::now());
            // A concurrent offer may win the freed slot; the hard cap still
            // applies and dropping this context then closes its idle endpoint.
            drop(self.insert_or_replace(key, context));
        }
    }

    fn insert_or_replace(
        &self,
        key: OriginH3PoolKey,
        context: Arc<OriginH3Connection>,
    ) -> Result<(), (OriginH3PoolKey, Arc<OriginH3Connection>)> {
        match self.entries.entry(key) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().last_used = Instant::now();
                let old = std::mem::replace(&mut entry.get_mut().context, context);
                drop(entry);
                drop(old);
            }
            Entry::Vacant(entry) => {
                if let Ok(slot) = self.slots.clone().try_acquire_owned() {
                    entry.insert(PooledH3Connection {
                        context,
                        last_used: Instant::now(),
                        _slot: slot,
                    });
                } else {
                    return Err((entry.into_key(), context));
                }
            }
        }
        Ok(())
    }

    fn maybe_sweep_at(&self, now: Instant) {
        if self.sweep.claim(now) {
            self.sweep_at(now);
        }
    }

    fn sweep_at(&self, now: Instant) {
        // Active contexts were removed by take(). The removed entries release
        // their own permits, so concurrent releases cannot overshoot the cap.
        self.entries
            .retain(|_, entry| !idle_expired(entry.last_used, now) && entry.context.is_open());
    }
}

static H3_CONNECTION_POOL: LazyLock<H3ConnectionPool> = LazyLock::new(H3ConnectionPool::new);

pub fn take(key: &OriginH3PoolKey) -> Option<Arc<OriginH3Connection>> {
    H3_CONNECTION_POOL.take(key)
}

pub fn offer(key: OriginH3PoolKey, context: Arc<OriginH3Connection>) {
    H3_CONNECTION_POOL.offer(key, context);
}

fn idle_expired(last_used: Instant, now: Instant) -> bool {
    now.saturating_duration_since(last_used) > H3_POOL_IDLE
}

fn sweep_idle_at(now: Instant) {
    #[cfg(test)]
    SWEEP_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Only pool-owned entries are touched. A connection is removed from the
    // pool the moment a request takes it, so active requests can never be
    // evicted here.
    H3_CONNECTION_POOL.sweep_at(now);
}

/// Owner-initiated cleanup for the idle connection pool. Called from the
/// High/Critical memory reclaim so idle
/// endpoints are released even when no further traffic arrives.
pub fn sweep_idle() {
    sweep_idle_at(Instant::now());
}

#[cfg(test)]
static SWEEP_RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
pub fn sweep_runs() -> u64 {
    SWEEP_RUNS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
pub(crate) fn expire_for_test(key: &OriginH3PoolKey) {
    H3_CONNECTION_POOL
        .entries
        .get_mut(key)
        .expect("pooled test context")
        .last_used = Instant::now() - H3_POOL_IDLE - Duration::from_secs(1);
}

#[cfg(test)]
pub(crate) fn available_slots_for_test() -> usize {
    H3_CONNECTION_POOL.slots.available_permits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_sweep_is_bounded_and_claimed_once_per_interval() {
        let schedule = SweepSchedule::new();
        let base = schedule.epoch;
        assert!(schedule.claim(base));
        assert!(!schedule.claim(base));
        assert!(!schedule.claim(base + Duration::from_millis(999)));
        assert!(schedule.claim(base + Duration::from_millis(1_000)));
        assert!(!schedule.claim(base + Duration::from_millis(1_000)));
        assert!(!schedule.claim(base));
        assert!(schedule.claim(base + Duration::from_secs(300)));
    }

    #[test]
    fn concurrent_sweep_claims_have_one_winner() {
        let schedule = SweepSchedule::new();
        let winners = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    if schedule.claim(schedule.epoch) {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn expired_key_is_rejected_even_with_a_deferred_scan() {
        crate::origin_h3::wire_tests::completed_pool_contexts(1, |mut contexts| {
            let (key, context) = contexts.pop().unwrap();
            let pool = H3ConnectionPool::new();
            pool.offer(key.clone(), context);
            pool.entries.get_mut(&key).unwrap().last_used =
                Instant::now() - H3_POOL_IDLE - Duration::from_secs(1);
            pool.sweep.next_ms.store(u64::MAX, Ordering::Relaxed);
            assert!(pool.take(&key).is_none());
            assert_eq!(pool.slots.available_permits(), H3_POOL_MAX_ENTRIES);
        });
    }

    #[test]
    fn capacity_pressure_reclaims_expired_entries_without_a_shard_lock() {
        crate::origin_h3::wire_tests::completed_pool_contexts(2, |mut contexts| {
            let (second_key, second) = contexts.pop().unwrap();
            let (first_key, first) = contexts.pop().unwrap();
            let weak = Arc::downgrade(&first);
            let mut pool = H3ConnectionPool::new();
            pool.slots = Arc::new(Semaphore::new(1));
            pool.offer(first_key.clone(), first.clone());
            pool.offer(first_key.clone(), first);
            assert_eq!(
                pool.slots.available_permits(),
                0,
                "replacement retains its slot"
            );
            pool.entries.get_mut(&first_key).unwrap().last_used =
                Instant::now() - H3_POOL_IDLE - Duration::from_secs(1);
            pool.sweep.next_ms.store(u64::MAX, Ordering::Relaxed);
            pool.offer(second_key.clone(), second);
            assert!(weak.upgrade().is_none());
            assert_eq!(pool.entries.len(), 1);
            assert_eq!(pool.slots.available_permits(), 0);
            drop(pool.take(&second_key).unwrap());
            assert_eq!(pool.slots.available_permits(), 1);
        });
    }

    #[test]
    fn capacity_refuses_new_idle_context_without_displacing_a_healthy_entry() {
        crate::origin_h3::wire_tests::completed_pool_contexts(2, |mut contexts| {
            let (second_key, second) = contexts.pop().unwrap();
            let (first_key, first) = contexts.pop().unwrap();
            let weak = Arc::downgrade(&second);
            let mut pool = H3ConnectionPool::new();
            pool.slots = Arc::new(Semaphore::new(1));
            pool.offer(first_key.clone(), first);
            pool.offer(second_key.clone(), second);
            assert!(weak.upgrade().is_none());
            assert!(pool.take(&second_key).is_none());
            assert_eq!(pool.entries.len(), 1);
            assert_eq!(pool.slots.available_permits(), 0);
            drop(pool.take(&first_key).unwrap());
            assert_eq!(pool.slots.available_permits(), 1);
        });
    }

    #[test]
    fn pool_sweep_removes_stale_entries_without_panicking() {
        let pool = H3ConnectionPool::new();
        pool.sweep_at(Instant::now());
        assert!(pool.entries.is_empty());
        assert_eq!(pool.slots.available_permits(), H3_POOL_MAX_ENTRIES);
    }

    #[test]
    fn idle_expiry_boundary_matches_pool_ttl() {
        let base = Instant::now();
        assert!(!idle_expired(base, base), "a fresh entry is not idle");
        assert!(
            !idle_expired(base, base + H3_POOL_IDLE),
            "an entry exactly at the TTL boundary stays pooled"
        );
        assert!(
            idle_expired(base, base + H3_POOL_IDLE + Duration::from_millis(1)),
            "an entry past the TTL is idle and swept"
        );
    }

    #[test]
    fn owner_sweep_is_callable_without_pooled_entries() {
        let before = sweep_runs();
        sweep_idle();
        sweep_idle();
        assert!(sweep_runs() >= before + 2);
        // Wire tests may use the global pool concurrently. Empty-table
        // assertions belong to a pool owned by this test.
        let pool = H3ConnectionPool::new();
        pool.sweep_at(Instant::now());
        assert!(pool.entries.is_empty());
    }
}
