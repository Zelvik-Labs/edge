use dashmap::DashMap;
use quinn::{Connection, Endpoint};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::origin_h3_state::OriginH3Key;

const H3_POOL_IDLE: Duration = Duration::from_secs(120);
const H3_POOL_MAX_ENTRIES: usize = 4_096;

struct PooledH3Connection {
    endpoint: Endpoint,
    connection: Connection,
    last_used: Instant,
}

static H3_CONNECTION_POOL: LazyLock<DashMap<OriginH3Key, PooledH3Connection>> =
    LazyLock::new(DashMap::new);

pub fn take(key: &OriginH3Key) -> Option<(Endpoint, Connection)> {
    sweep_idle();
    let (_, pooled) = H3_CONNECTION_POOL.remove(key)?;
    if pooled.connection.close_reason().is_some() {
        return None;
    }
    Some((pooled.endpoint, pooled.connection))
}

pub fn offer(key: OriginH3Key, endpoint: Endpoint, connection: Connection) {
    if connection.close_reason().is_some() {
        return;
    }
    sweep_idle();
    if H3_CONNECTION_POOL.len() >= H3_POOL_MAX_ENTRIES {
        return;
    }
    H3_CONNECTION_POOL.insert(
        key,
        PooledH3Connection {
            endpoint,
            connection,
            last_used: Instant::now(),
        },
    );
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
    H3_CONNECTION_POOL.retain(|_, entry| {
        !idle_expired(entry.last_used, now) && entry.connection.close_reason().is_none()
    });
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
mod tests {
    use super::*;

    #[test]
    fn pool_sweep_removes_stale_entries_without_panicking() {
        sweep_idle();
        assert!(H3_CONNECTION_POOL.is_empty());
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
        sweep_idle();
        sweep_idle();
        assert!(H3_CONNECTION_POOL.is_empty());
    }
}
