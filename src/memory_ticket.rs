//! Per-connection admission tickets (request-workspace pool).
//!
//! Request-scoped admissions (request/response body WAF, response
//! transforms, H2 streams) previously each performed their own global
//! counter RMW per request. Under sustained QPS those adds are pure
//! cacheline contention: every core hammers the same padded counter for
//! work that is private to one connection.
//!
//! A `TicketBucket` is attached to each registered L4 connection (via
//! `L4ConnectionRegistry`'s `by_addr` index) and holds a pool of bytes
//! checked out from the governor's `request_workspace` ledger once, in
//! chunks. Request-scoped classes then *spend* from the bucket with a local
//! CAS on a connection-private line — uncontended, ~5ns — and only an empty
//! bucket reaches the global ledger.
//!
//! # Ownership and limits
//!
//! A bucket belongs to its registration, not intrinsically to a socket or
//! request. The L4 registry shares it across requests of that registration.
//! Its total held bytes (including idle float) are capped before global refill;
//! the global workspace and physical shared account remain independent gates.
//! Large WAF/transform work cannot consume the protected continuation reserve.
//! H2 streams hold ordinary class permits instead of workspace float, enforcing
//! the same global count and byte gates on pooled and unregistered paths.
//!
//! # Float and refund policy
//!
//! - Refill draws `need + float` where `float` is the governor's
//!   pressure-scaled slack (256KiB at Normal/Elevated, shrinking under
//!   High and zero under Critical) so a stream of small charges
//!   (small WAF buffers, response transforms) amortizes to one global op
//!   per ~`float/charge` spends. If the float top-up cannot be afforded the
//!   refill retries with the bare need, then fails closed. H2 streams do
//!   not draw float: they take an ordinary class permit and release it
//!   when the stream ends.
//! - `release` returns bytes to the bucket; once the balance exceeds the
//!   governor's pressure-scaled float cap (1MiB at Normal, 256KiB at
//!   High, 0 at Critical) the excess is refunded to the global ledger —
//!   idle workspace is reclaimable capacity, not a per-connection
//!   reservation, and pressure pulls it back without evicting in-flight
//!   work.
//! - Dropping the bucket (connection close) refunds everything still held.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::memory_governor::{
    AdmissionClass, MEMORY_GOVERNOR, MemoryGovernor, StaticAdmissionPermit,
};

tokio::task_local! {
    pub(crate) static CONNECTION_TICKET: Arc<TicketBucket>;
}

/// Explicit owner handoff for protocols (QUIC) without a stable peer-address key.
pub(crate) fn current_connection_ticket() -> Option<Arc<TicketBucket>> {
    CONNECTION_TICKET.try_with(Arc::clone).ok()
}

/// Per-owner pool of pre-admitted workspace bytes.
pub struct TicketBucket {
    /// Spendable bytes currently held from the workspace ledger.
    balance: AtomicU64,
    /// Total bytes checked out (balance + in-flight spends); refunded on drop.
    held: AtomicU64,
    governor: &'static MemoryGovernor,
}

impl TicketBucket {
    pub fn new(governor: &'static MemoryGovernor) -> Self {
        Self {
            balance: AtomicU64::new(0),
            held: AtomicU64::new(0),
            governor,
        }
    }

    /// Draw `bytes` from the local pool without touching the global ledger.
    /// CAS loop on a connection-private line — uncontended in the common
    /// case (HTTP/2 streams on one connection can race, so CAS is required).
    fn try_draw(&self, bytes: u64) -> bool {
        let mut cur = self.balance.load(Ordering::Acquire);
        loop {
            if cur < bytes {
                return false;
            }
            match self.balance.compare_exchange_weak(
                cur,
                cur - bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Pull `need` bytes (plus float margin when affordable) from the global
    /// workspace ledger into the local pool.
    fn refill(&self, need: u64) -> bool {
        let want = need.saturating_add(self.governor.ticket_float_bytes());
        self.refill_exact(want) || (want != need && self.refill_exact(need))
    }

    fn refill_exact(&self, bytes: u64) -> bool {
        let cap = self.governor.ticket_owner_limit_bytes();
        // Reserve the owner's held bytes before touching the global ledger.
        // Concurrent refills may not each observe the same unused allowance.
        if self
            .held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                held.checked_add(bytes).filter(|next| *next <= cap)
            })
            .is_err()
        {
            return false;
        }
        if !self.governor.try_admit_workspace(bytes) {
            self.held.fetch_sub(bytes, Ordering::AcqRel);
            return false;
        }
        self.balance.fetch_add(bytes, Ordering::AcqRel);
        true
    }

    /// Spend `bytes` from the pool for `class`, refilling from the global
    /// ledger when the local balance cannot cover it. Returns `None` (and
    /// records the class reject) only when the global ledger cannot cover
    /// the spend — identical fail-closed contract to `try_admit`.
    pub fn spend(self: &Arc<Self>, class: AdmissionClass, bytes: u64) -> Option<TicketPermit> {
        // A stream slot is a node-wide concurrency limit, not reusable byte
        // float. Hold the ordinary class permit for exactly the stream lifetime;
        // its account bytes are already charged, so do not also refill workspace.
        if class == AdmissionClass::Http2Stream {
            return self.governor.try_admit(class).map(|permit| TicketPermit {
                bucket: Arc::clone(self),
                bytes: 0,
                _stream: Some(permit),
            });
        }
        let bytes = bytes.max(1);
        // Bounded retries: a racing stream on the same connection could
        // drain a fresh refill before we get to draw from it.
        for _ in 0..3 {
            if self.try_draw(bytes) {
                return Some(TicketPermit {
                    bucket: Arc::clone(self),
                    bytes,
                    _stream: None,
                });
            }
            let cur = self.balance.load(Ordering::Relaxed);
            if !self.refill(bytes.saturating_sub(cur)) {
                break;
            }
        }
        self.governor.record_reject(class);
        None
    }

    /// Return `bytes` to the pool; refund the excess over the current
    /// pressure-scaled float cap to the global ledger so big one-off
    /// charges (a 16MiB transform) are not idle-held for the connection's
    /// remaining lifetime, and idle slack returns under pressure.
    fn release(&self, bytes: u64) {
        let cap = self.governor.ticket_float_cap_bytes();
        let mut cur = self.balance.fetch_add(bytes, Ordering::AcqRel) + bytes;
        while cur > cap {
            let excess = cur - cap;
            match self.balance.compare_exchange_weak(
                cur,
                cur - excess,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.held.fetch_sub(excess, Ordering::AcqRel);
                    self.governor.workspace_refund(excess);
                    break;
                }
                Err(observed) => cur = observed,
            }
        }
    }

    /// Bytes currently held from the workspace ledger (observability).
    pub fn held_bytes(&self) -> u64 {
        self.held.load(Ordering::Relaxed)
    }
}

impl Drop for TicketBucket {
    fn drop(&mut self) {
        let held = self.held.swap(0, Ordering::AcqRel);
        if held > 0 {
            self.governor.workspace_refund(held);
        }
    }
}

/// RAII release of a pooled spend: dropping returns the bytes to the
/// owning bucket (which may refund the global ledger past the float cap).
pub struct TicketPermit {
    bucket: Arc<TicketBucket>,
    bytes: u64,
    _stream: Option<StaticAdmissionPermit>,
}

impl Drop for TicketPermit {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.bucket.release(self.bytes);
        }
    }
}

/// Permit type stored in `ProxyCTX` for pooled classes: either a bucket
/// spend (registered connection) or a plain class admission (paths where no
/// connection ticket exists, e.g. unregistered sessions).
pub enum WorkspacePermit {
    Pooled(TicketPermit),
    Direct(StaticAdmissionPermit),
}

/// Pooled admission for request-scoped classes: draw from the connection's
/// ticket bucket when one exists, else fall back to a direct class admit so
/// unregistered paths keep identical semantics.
pub fn admit_pooled(
    bucket: Option<&Arc<TicketBucket>>,
    class: AdmissionClass,
    bytes: u64,
) -> Option<WorkspacePermit> {
    match bucket {
        Some(b) => b.spend(class, bytes).map(WorkspacePermit::Pooled),
        None => MEMORY_GOVERNOR
            .try_admit(class)
            .map(WorkspacePermit::Direct),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_governor::MemoryGovernor;

    fn leaked_governor(available: u64) -> &'static MemoryGovernor {
        let g: &'static MemoryGovernor = Box::leak(Box::new(MemoryGovernor::new()));
        let now = crate::utils::time::system_timestamp_millis().max(0) as u64;
        // Seed the cached-memory inputs directly; seed_governor_memory in
        // governor tests is module-private.
        g.seed_cached_for_test(available * 4, available, 65536, 0, now);
        g
    }

    /// Leaked governor seeded with a synthetic observation frozen at
    /// `i64::MAX`. `snapshot_usable` sees a saturated-negative age for a
    /// future timestamp, so `limits()`/`snapshot()` never re-read host
    /// memory and every derived budget is deterministic for the whole test.
    fn frozen_governor(available: u64) -> &'static MemoryGovernor {
        let g: &'static MemoryGovernor = Box::leak(Box::new(MemoryGovernor::new()));
        g.seed_cached_for_test(available * 4, available, 65536, 0, i64::MAX as u64);
        g
    }

    #[test]
    fn bucket_spends_locally_after_first_refill() {
        let g = leaked_governor(256 * 1024 * 1024);
        let bucket = Arc::new(TicketBucket::new(g));

        let p1 = bucket
            .spend(AdmissionClass::RequestBodyWaf, 16 * 1024)
            .expect("first spend refills");
        let held_after = bucket.held_bytes();
        assert!(held_after >= 16 * 1024);
        let ledger_after_first = g.request_workspace_used_bytes();
        assert_eq!(ledger_after_first, held_after);

        // Second small spend draws locally — global ledger unchanged.
        let p2 = bucket
            .spend(AdmissionClass::RequestBodyWaf, 16 * 1024)
            .expect("second spend is local");
        assert_eq!(g.request_workspace_used_bytes(), ledger_after_first);

        drop(p1);
        drop(p2);
        // 32KiB back in balance, under the cap — bucket keeps it.
        assert!(bucket.held_bytes() >= 32 * 1024);
    }

    #[test]
    fn release_refunds_excess_over_cap() {
        let g = leaked_governor(256 * 1024 * 1024);
        let bucket = Arc::new(TicketBucket::new(g));
        let big = 8 * 1024 * 1024; // > Normal-level float cap
        let p = bucket
            .spend(AdmissionClass::ResponseTransform, big)
            .expect("big spend refills");
        let held_in_flight = bucket.held_bytes();
        drop(p);
        // After release, idle balance is capped at the (Normal) float cap.
        assert!(bucket.held_bytes() <= g.ticket_float_cap_bytes());
        assert!(bucket.held_bytes() < held_in_flight);
    }

    #[test]
    fn spend_fails_closed_when_ledger_full() {
        let g = leaked_governor(64 * 1024);
        let bucket = Arc::new(TicketBucket::new(g));
        let need = 512 * 1024 * 1024; // far beyond any budget
        assert!(bucket.spend(AdmissionClass::RequestBodyWaf, need).is_none());
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    #[test]
    fn bucket_drop_refunds_held() {
        let g = leaked_governor(256 * 1024 * 1024);
        let bucket = Arc::new(TicketBucket::new(g));
        let p = bucket
            .spend(AdmissionClass::RequestBodyWaf, 16 * 1024)
            .unwrap();
        drop(p);
        let held = bucket.held_bytes();
        assert!(held > 0);
        drop(bucket); // last Arc besides the permit's? permit dropped already
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    #[test]
    fn unregistered_path_falls_back_to_direct_admit() {
        let g = leaked_governor(256 * 1024 * 1024);
        let _ = g; // admit_pooled(None,...) uses MEMORY_GOVERNOR global;
        // just assert the fallback shape — the global governor may or may
        // not be budgeted in tests, but a None bucket never panics.
        let permit = admit_pooled(None, AdmissionClass::RequestBodyWaf, 2 * 1024 * 1024);
        match permit {
            Some(WorkspacePermit::Direct(_)) | None => {}
            Some(WorkspacePermit::Pooled(_)) => panic!("None bucket must not pool"),
        }
    }

    #[test]
    fn single_owner_cannot_starve_independent_workspace() {
        let g = frozen_governor(256 * 1024 * 1024);
        let attacker = Arc::new(TicketBucket::new(g));
        let mut held = Vec::new();
        while let Some(permit) = attacker.spend(AdmissionClass::RequestBodyWaf, 2 << 20) {
            held.push(permit);
            assert!(held.len() < 100);
        }
        assert!(attacker.held_bytes() <= g.ticket_owner_limit_bytes());
        assert!(attacker.held_bytes() > 0);
        let victim = Arc::new(TicketBucket::new(g));
        let victim_permit = victim
            .spend(AdmissionClass::RequestBodyWaf, 16 << 10)
            .expect("one owner must leave space for independent work");
        drop((held, victim_permit, attacker, victim));
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    #[test]
    fn small_workspace_owner_cannot_take_transform_floor() {
        let g = frozen_governor(64 << 20);
        let attacker = Arc::new(TicketBucket::new(g));
        assert_eq!(g.ticket_owner_limit_bytes(), 4 << 20);
        assert!(
            attacker
                .spend(AdmissionClass::ResponseTransform, 16 << 20)
                .is_none()
        );
        let mut held = Vec::new();
        while let Some(permit) = attacker.spend(AdmissionClass::RequestBodyWaf, 2 << 20) {
            held.push(permit);
        }
        assert!(!held.is_empty());
        let victim = Arc::new(TicketBucket::new(g));
        let victim_permit = victim
            .spend(AdmissionClass::RequestBodyWaf, 16 << 10)
            .expect("a small workspace must retain independent owner capacity");
        drop((held, victim_permit, attacker, victim));
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    #[test]
    fn concurrent_refills_respect_owner_cap_and_refund() {
        let g = frozen_governor(256 << 20);
        let bucket = Arc::new(TicketBucket::new(g));
        let barrier = std::sync::Barrier::new(8);
        let held = std::thread::scope(|scope| {
            let barrier = &barrier;
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    let bucket = &bucket;
                    scope.spawn(move || {
                        barrier.wait();
                        let mut permits = Vec::new();
                        while let Some(p) = bucket.spend(AdmissionClass::RequestBodyWaf, 2 << 20) {
                            permits.push(p);
                        }
                        permits
                    })
                })
                .collect();
            jobs.into_iter()
                .flat_map(|j| j.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(bucket.held_bytes() <= g.ticket_owner_limit_bytes());
        assert_eq!(g.request_workspace_used_bytes(), bucket.held_bytes());
        drop((held, bucket));
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    #[test]
    fn pooled_h2_streams_enforce_global_count_and_release() {
        let g = frozen_governor(512 << 20);
        let limit = g.limit_for(AdmissionClass::Http2Stream);
        assert!((1_024..=8_192).contains(&limit));
        let bucket = Arc::new(TicketBucket::new(g));
        let mut held = Vec::new();
        for _ in 0..limit {
            held.push(
                bucket
                    .spend(AdmissionClass::Http2Stream, 16 << 10)
                    .expect("fixture must reach global stream count"),
            );
        }
        assert!(
            bucket
                .spend(AdmissionClass::Http2Stream, 16 << 10)
                .is_none()
        );
        assert_eq!(g.snapshot(1).estimated_h2_streams, limit as u64);
        assert_eq!(
            g.connection_admission_used_bytes(),
            limit as u64 * (16 << 10)
        );
        assert_eq!(g.request_workspace_used_bytes(), 0, "no double byte charge");
        held.pop();
        let replacement = bucket
            .spend(AdmissionClass::Http2Stream, 16 << 10)
            .expect("drop returns the global slot");
        drop((held, replacement, bucket));
        assert_eq!(g.snapshot(1).estimated_h2_streams, 0);
        assert_eq!(g.connection_admission_used_bytes(), 0);
    }
}
