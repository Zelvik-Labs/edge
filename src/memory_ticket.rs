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
//! # Semantics (approved isolation change)
//!
//! Per-class isolation is replaced by a per-connection pool: inside a
//! connection, classes share the bucket, so heavy use of one (say request
//! body WAF) can starve another (transform). Buckets have no in-flight byte
//! ceiling: one connection can also exhaust the global workspace ledger and
//! deny another connection's refill. Pooled H2 spends do not enforce the
//! direct path's global stream counter. The audit_known_gap tests below
//! reproduce both limitations.
//! Failure semantics at each call site are unchanged: a spend that cannot
//! be satisfied returns `None`, which is exactly what `try_admit` returned,
//! and the class's own reject counter is still incremented.
//!
//! # Float and refund policy
//!
//! - Refill draws `need + float` where `float` is the governor's
//!   pressure-scaled slack (256KiB at Normal/Elevated, shrinking under
//!   High and zero under Critical) so a stream of small charges
//!   (H2 streams, small WAF buffers) amortizes to one global op per
//!   ~`float/charge` spends. If the float top-up cannot be afforded the
//!   refill retries with the bare need, then fails closed.
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

/// Per-connection pool of pre-admitted workspace bytes.
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
        if self.governor.try_admit_workspace(want) {
            self.held.fetch_add(want, Ordering::AcqRel);
            self.balance.fetch_add(want, Ordering::AcqRel);
            return true;
        }
        // Float unaffordable: still try the bare need so a tight ledger
        // degrades to per-spend global accounting instead of rejecting.
        if self.governor.try_admit_workspace(need) {
            self.held.fetch_add(need, Ordering::AcqRel);
            self.balance.fetch_add(need, Ordering::AcqRel);
            return true;
        }
        false
    }

    /// Spend `bytes` from the pool for `class`, refilling from the global
    /// ledger when the local balance cannot cover it. Returns `None` (and
    /// records the class reject) only when the global ledger cannot cover
    /// the spend — identical fail-closed contract to `try_admit`.
    pub fn spend(self: &Arc<Self>, class: AdmissionClass, bytes: u64) -> Option<TicketPermit> {
        let bytes = bytes.max(1);
        // Bounded retries: a racing stream on the same connection could
        // drain a fresh refill before we get to draw from it.
        for _ in 0..3 {
            if self.try_draw(bytes) {
                return Some(TicketPermit {
                    bucket: Arc::clone(self),
                    bytes,
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
}

impl Drop for TicketPermit {
    fn drop(&mut self) {
        self.bucket.release(self.bytes);
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
            .spend(AdmissionClass::Http2Stream, 16 * 1024)
            .expect("first spend refills");
        let held_after = bucket.held_bytes();
        assert!(held_after >= 16 * 1024);
        let ledger_after_first = g.request_workspace_used_bytes();
        assert_eq!(ledger_after_first, held_after);

        // Second small spend draws locally — global ledger unchanged.
        let p2 = bucket
            .spend(AdmissionClass::Http2Stream, 16 * 1024)
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
            .spend(AdmissionClass::Http2Stream, 16 * 1024)
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

    /// KNOWN GAP — documents current behavior; this test passes as written.
    ///
    /// A `TicketBucket` has no ceiling on in-flight `held` bytes (`release`
    /// only refunds the idle balance above the float cap) and every bucket
    /// refills from the single global `request_workspace` ledger. One
    /// accepted connection can therefore drain that ledger and deny every
    /// other connection's request-scoped admissions.
    ///
    /// Fixture: seeded available = 256 MiB, total = 1 GiB (Normal pressure),
    /// so the workspace ledger is available/4 = 64 MiB while the Ordinary
    /// account capacity (~122 MiB) remains larger. The victim's rejection is
    /// therefore the workspace ledger's, not the shared account's.
    #[test]
    fn audit_known_gap_single_bucket_exhausts_global_workspace() {
        let g = frozen_governor(256 * 1024 * 1024);
        let workspace_budget: u64 = 256 * 1024 * 1024 / 4;
        assert_eq!(workspace_budget, 64 * 1024 * 1024);

        let attacker = Arc::new(TicketBucket::new(g));
        let mut held = Vec::new();

        // 2 MiB is the RequestBodyWaf estimate and the coarsest realistic
        // pooled charge; ~30 permits consume most of the 64 MiB ledger.
        while let Some(permit) = attacker.spend(AdmissionClass::RequestBodyWaf, 2 * 1024 * 1024) {
            held.push(permit);
            assert!(held.len() < 1_000, "fixture must stay bounded");
        }

        // Drain the sub-2MiB remainder with the smallest pooled class. The
        // bare-need refill fallback lets a 16 KiB spend through while at
        // least 16 KiB remains, so keep going until even that fails.
        while let Some(permit) = attacker.spend(AdmissionClass::Http2Stream, 16 * 1024) {
            held.push(permit);
            assert!(held.len() < 2_000, "fixture must stay bounded");
        }

        let used = g.request_workspace_used_bytes();
        assert!(used <= workspace_budget, "ledger overshoot: used={used}");
        assert!(
            workspace_budget - used < 16 * 1024,
            "one bucket consumed the shared ledger: used={used}"
        );
        // The account still has room, so the rejection below is the
        // workspace ledger's, not the shared account's.
        assert!(g.account_view().grantable_bytes > 0);

        // KNOWN GAP: a victim connection cannot admit even one 16 KiB stream.
        let victim = Arc::new(TicketBucket::new(g));
        assert!(
            victim
                .spend(AdmissionClass::Http2Stream, 16 * 1024)
                .is_none(),
            "current behavior: one connection's pool denies every other connection"
        );

        drop(held);
        drop(attacker);
        drop(victim);
        // Releasing every permit and the bucket refunds the whole ledger.
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }

    /// KNOWN GAP — documents current behavior; this test passes as written.
    ///
    /// `admit_pooled` with a `TicketBucket` spends only from
    /// `request_workspace` and never increments `h2_streams`, so the
    /// materialized `h2_stream_global_limit` is enforced only on the direct
    /// `try_admit` path. Every registered H2 connection owns a bucket
    /// (`L4ConnectionRegistry::register`), so the H2 global count limit is
    /// effectively unenforced and `estimated_h2_streams` stays 0.
    ///
    /// Fixture: seeded available = 512 MiB, total = 2 GiB. Availability is
    /// Unknown (the seeded observation does not set it), so the materialized
    /// H2 limit is max((connection_budget/8)/16 KiB, cpu_parallelism*4096)
    /// and cpu_parallelism defaults to 1 here: the limit is 4096. The
    /// workspace ledger (available/4 = 128 MiB) and the Ordinary account
    /// capacity (~346 MiB) both cover 4097 x 16 KiB (~64 MiB of refills).
    #[test]
    fn audit_known_gap_pooled_h2_streams_bypass_global_count_limit() {
        let g = frozen_governor(512 * 1024 * 1024);
        let global_limit = g.limit_for(AdmissionClass::Http2Stream);
        assert!(
            (1_024..=8_192).contains(&global_limit),
            "fixture must keep the permit loop bounded, got {global_limit}"
        );

        let bucket = Arc::new(TicketBucket::new(g));
        let mut held = Vec::new();
        for _ in 0..=global_limit {
            let permit = bucket
                .spend(AdmissionClass::Http2Stream, 16 * 1024)
                .expect("pooled spend ignores the global class limit");
            held.push(permit);
        }

        // KNOWN GAP: one more stream than the global limit exists, yet the
        // class counter and the shared connection ledger are still zero.
        assert_eq!(held.len(), global_limit + 1);
        let snapshot = g.snapshot(1);
        assert_eq!(
            snapshot.estimated_h2_streams, 0,
            "pooled spends never touch the h2_streams counter"
        );
        assert_eq!(
            snapshot.connection_admission_used_bytes, 0,
            "shared connection ledger is blind to pooled streams"
        );

        drop(held);
        drop(bucket);
        assert_eq!(g.request_workspace_used_bytes(), 0);
    }
}
