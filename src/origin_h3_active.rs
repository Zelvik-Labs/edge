//! Bounded, non-owning index of active H3 origin protocol contexts.
//! Idle retention remains the responsibility of origin_h3_pool. An index entry
//! cannot keep a connection alive after its last request/idle owner disappears.
use dashmap::{DashMap, mapref::entry::Entry};
use std::sync::{Arc, LazyLock, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::origin_h3::{OriginH3Connection, OriginH3PoolKey};

const MAX_TRACKED_CONTEXTS: usize = 4096;

struct ActiveEntry {
    context: Weak<OriginH3Connection>,
    _slot: OwnedSemaphorePermit,
}

struct ActivePool {
    entries: DashMap<OriginH3PoolKey, ActiveEntry>,
    slots: Arc<Semaphore>,
}

static ACTIVE: LazyLock<ActivePool> = LazyLock::new(|| ActivePool {
    entries: DashMap::new(),
    slots: Arc::new(Semaphore::new(MAX_TRACKED_CONTEXTS)),
});

pub(crate) fn register(key: &OriginH3PoolKey, context: &Arc<OriginH3Connection>) {
    if ACTIVE
        .entries
        .get(key)
        .is_some_and(|entry| entry.context.as_ptr() == Arc::as_ptr(context))
    {
        return;
    }
    // Never upgrade a Weak while holding a map guard: dropping that upgrade
    // could be the final strong reference and re-enter remove on this shard.
    match ACTIVE.entries.entry(key.clone()) {
        Entry::Occupied(mut entry) => entry.get_mut().context = Arc::downgrade(context),
        Entry::Vacant(entry) => {
            if let Ok(slot) = ACTIVE.slots.clone().try_acquire_owned() {
                entry.insert(ActiveEntry {
                    context: Arc::downgrade(context),
                    _slot: slot,
                });
            }
        }
    }
}

pub(crate) fn get(key: &OriginH3PoolKey) -> Option<Arc<OriginH3Connection>> {
    #[cfg(test)]
    if !ACTIVE_REUSE_ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let weak = ACTIVE.entries.get(key).map(|entry| entry.context.clone())?;
    let context = weak.upgrade()?;
    context.can_open_request().then_some(context)
}

pub(crate) fn remove(key: &OriginH3PoolKey, context: *const OriginH3Connection) {
    // A newer connection for the same policy key may have replaced this one.
    ACTIVE
        .entries
        .remove_if(key, |_, entry| entry.context.as_ptr() == context);
}

#[cfg(test)]
pub(crate) fn slots_for_test() -> usize {
    ACTIVE.slots.available_permits()
}

#[cfg(test)]
static ACTIVE_REUSE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

#[cfg(test)]
pub(crate) fn set_probe_reuse(enabled: bool) {
    ACTIVE_REUSE_ENABLED.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
