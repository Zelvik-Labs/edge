use dashmap::{DashMap, mapref::entry::Entry};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::origin_h3::{OriginH3Connection, OriginH3PoolKey};

const H3_POOL_IDLE: Duration = Duration::from_secs(120);
const H3_POOL_MAX_ENTRIES: usize = 4_096;

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
}

impl H3ConnectionPool {
    fn new() -> Self {
        Self {
            entries: DashMap::new(),
            slots: Arc::new(Semaphore::new(H3_POOL_MAX_ENTRIES)),
        }
    }

    fn take(&self, key: &OriginH3PoolKey) -> Option<Arc<OriginH3Connection>> {
        self.sweep_at(Instant::now());
        let (_, pooled) = self.entries.remove(key)?;
        pooled.context.is_open().then_some(pooled.context)
    }

    fn offer(&self, key: OriginH3PoolKey, context: Arc<OriginH3Connection>) {
        if !context.is_open() {
            return;
        }
        self.sweep_at(Instant::now());
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
                }
            }
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
/// request path (take/offer) and from High/Critical memory reclaim so idle
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
