use dashmap::DashMap;
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

const MAX_ORIGIN_STATES: usize = 512;
const FAILS_TO_DOWN: u32 = 5;
const AUTO_RECOVER_SECONDS: i64 = 60;

pub static ORIGIN_STATE_MANAGER: Lazy<OriginStateManager> = Lazy::new(OriginStateManager::new);

pub struct OriginState {
    fail_count: AtomicU32,
    is_down: AtomicBool,
    updated_at: AtomicI64,
}

impl OriginState {
    fn new() -> Self {
        Self {
            fail_count: AtomicU32::new(0),
            is_down: AtomicBool::new(false),
            updated_at: AtomicI64::new(now_seconds()),
        }
    }
}

pub struct OriginStateManager {
    states: DashMap<i64, Arc<OriginState>>,
    inflight: Arc<OriginInflightCounters>,
}

impl OriginStateManager {
    fn new() -> Self {
        Self {
            states: DashMap::with_shard_amount(64),
            inflight: Arc::new(OriginInflightCounters::new()),
        }
    }

    /// Shared per-origin in-flight request counters, used by the
    /// least-connections load balancer. Cloning the returned `Arc` keeps the
    /// counters alive for the lifetime of a built load balancer.
    pub fn inflight_counters(&self) -> Arc<OriginInflightCounters> {
        Arc::clone(&self.inflight)
    }

    /// Increment the in-flight counter for `origin_id` and return an RAII guard
    /// that decrements it on drop. The guard must be kept for the whole
    /// lifetime of the request so that every exit path (success, failure,
    /// cancellation) releases the slot exactly once.
    pub fn begin_inflight(&self, origin_id: i64) -> OriginInflightGuard {
        self.inflight.begin(origin_id)
    }

    pub fn record_success(&self, origin_id: i64) {
        if origin_id <= 0 {
            return;
        }
        if let Some(state) = self.states.get(&origin_id) {
            state.fail_count.store(0, Ordering::Relaxed);
            if state.is_down.swap(false, Ordering::Relaxed) {
                debug!("origin {} recovered after successful response", origin_id);
            }
            state.updated_at.store(now_seconds(), Ordering::Relaxed);
        }
    }

    pub fn record_failure(&self, origin_id: i64) {
        if origin_id <= 0 {
            return;
        }
        let state = self.get_or_insert(origin_id);
        let fails = state.fail_count.fetch_add(1, Ordering::Relaxed) + 1;
        state.updated_at.store(now_seconds(), Ordering::Relaxed);
        if fails >= FAILS_TO_DOWN && !state.is_down.swap(true, Ordering::Relaxed) {
            warn!("origin {} marked down after {} failures", origin_id, fails);
        }
    }

    pub fn is_down(&self, origin_id: i64) -> bool {
        if origin_id <= 0 {
            return false;
        }
        self.states
            .get(&origin_id)
            .map(|state| state.is_down.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn recover_expired(&self) {
        let now = now_seconds();
        for entry in self.states.iter() {
            let origin_id = *entry.key();
            let state = entry.value();
            if state.is_down.load(Ordering::Relaxed)
                && now.saturating_sub(state.updated_at.load(Ordering::Relaxed))
                    >= AUTO_RECOVER_SECONDS
            {
                state.fail_count.store(0, Ordering::Relaxed);
                state.is_down.store(false, Ordering::Relaxed);
                state.updated_at.store(now, Ordering::Relaxed);
                debug!("origin {} auto-recovered after down timeout", origin_id);
            }
        }
    }

    fn get_or_insert(&self, origin_id: i64) -> Arc<OriginState> {
        if let Some(state) = self.states.get(&origin_id) {
            return Arc::clone(&state);
        }
        if self.states.len() >= MAX_ORIGIN_STATES
            && let Some(key) = self.states.iter().next().map(|entry| *entry.key())
        {
            self.states.remove(&key);
        }
        Arc::clone(
            self.states
                .entry(origin_id)
                .or_insert_with(|| Arc::new(OriginState::new()))
                .value(),
        )
    }
}

/// Per-origin in-flight request counters shared between the least-connections
/// load balancer (which reads them to rank candidates) and the request path
/// (which increments/decrements them through [OriginInflightGuard]).
///
/// This is deliberately separate from `metrics::active_requests`, which is a
/// single global counter and cannot rank individual origins.
pub struct OriginInflightCounters {
    counts: DashMap<i64, Arc<AtomicUsize>>,
}

impl OriginInflightCounters {
    pub fn new() -> Self {
        Self {
            counts: DashMap::with_shard_amount(64),
        }
    }

    /// Current number of in-flight requests routed to `origin_id`.
    pub fn inflight(&self, origin_id: i64) -> usize {
        if origin_id <= 0 {
            return 0;
        }
        self.counts
            .get(&origin_id)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Increment the counter for `origin_id` and return a guard that decrements
    /// it on drop. A non-positive `origin_id` (e.g. the `127.0.0.1:80` fallback
    /// backend) yields a no-op guard.
    pub fn begin(&self, origin_id: i64) -> OriginInflightGuard {
        if origin_id <= 0 {
            return OriginInflightGuard { counter: None };
        }
        let counter = Arc::clone(
            self.counts
                .entry(origin_id)
                .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
                .value(),
        );
        counter.fetch_add(1, Ordering::Relaxed);
        OriginInflightGuard {
            counter: Some(counter),
        }
    }
}

impl Default for OriginInflightCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII handle that keeps one in-flight slot counted for the origin it was
/// created for. Dropping it releases exactly the slot taken by
/// [OriginInflightCounters::begin], so a leaked counter is impossible even on
/// early return, error, or cancellation.
pub struct OriginInflightGuard {
    counter: Option<Arc<AtomicUsize>>,
}

impl OriginInflightGuard {
    /// A guard that counts nothing. Handed to load-balancing algorithms that do
    /// not rank by in-flight count so their entry points can hold the same
    /// guard type without ever touching the shared counter map.
    pub fn disabled() -> Self {
        Self { counter: None }
    }
}

impl Drop for OriginInflightGuard {
    fn drop(&mut self) {
        if let Some(counter) = &self.counter {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub fn start_origin_state_cleanup_task() {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            ORIGIN_STATE_MANAGER.recover_expired();
        }
    });
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}
