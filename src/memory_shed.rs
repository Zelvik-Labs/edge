//! Bounded emergency shedding — the last lever before the OOM killer.
//!
//! Cooperative reclaim (`memory_reclaim`) shrinks caches and rejects new
//! admissions, but cannot touch work already in flight: a long download or a
//! lingering keepalive connection keeps its buffers until it finishes. Shed
//! escalation is driven from `memory_reclaim::on_memory_pressure_observed`:
//!
//! - `High`     → keepalive suppression: every response is written with
//!   keepalive disabled, so idle connections drain at the next request
//!   boundary instead of holding buffers open.
//! - `Critical` → keepalive suppression plus a **final-resort** drain pass,
//!   taken only when every one of these holds:
//!   * Critical has been observed continuously for at least 10s, with
//!     samples taken at least 5s apart (re-reading a stale snapshot cannot
//!     build up sustained evidence);
//!   * the latest reclaim pass ran within the last 30s and was measurably
//!     ineffective (RSS improvement < 4 MiB, or no cache entries/estimated
//!     bytes freed when RSS is unknown);
//!   * the shared account reports actual headroom (`Some`) that is strictly
//!     below the necessary working set — `connection_admission_used_bytes +
//!     request_workspace_used_bytes`. Unknown account data never triggers a
//!     kill, and a PSI/cgroup-event floor that the actual memory snapshot
//!     does not confirm never triggers one either.
//!
//!   A pass drains the oldest `SHED_BATCH_LIMIT` L4 connections and H3
//!   connections, skips records already being cancelled, consumes the
//!   ineffective-reclaim evidence (one pass per piece of evidence), and is
//!   rate-limited to at most one pass per 30s.
//!
//! New-connection admission already collapses toward its critical floor via
//! `pressure_adjusted_min_limit`; outright refusal is deliberately NOT added
//! here — a hard-refused connection produces no response at all, while shed
//! connections at least get their in-flight response finished (keepalive
//! tier) or a clean close (drain tier).
//!
//! De-escalation follows the same hysteresis as the reclaim coordinator:
//! keepalive suppression lifts only after the observed level held below
//! `High` for 30s, and the sustained-Critical episode (including its
//! exhaustion evidence) resets on the first sub-Critical sample. Everything
//! the proxy hot path reads is a relaxed atomic load; the governor snapshot
//! and drains run only on the reclaim monitor thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::l4_connection_registry::{self, ConnectionCancelReason};
use crate::memory_governor::MemoryPressureLevel;

/// Critical must hold continuously for this long before a drain is allowed.
const CRITICAL_SUSTAIN_MS: u64 = 10_000;

/// Samples closer together than this do not count as fresh evidence — a
/// burst of observations of the same snapshot must not accumulate sustain.
const CRITICAL_SAMPLE_MIN_INTERVAL_MS: u64 = 5_000;

/// Reclaim evidence older than this no longer justifies a final-resort pass.
const INEFFECTIVE_EVIDENCE_MAX_AGE_MS: u64 = 30_000;

/// Minimum interval between final-resort passes.
const FINAL_RESORT_MIN_INTERVAL_MS: u64 = 30_000;

/// Keepalive suppression is lifted only after the observed level has
/// stayed below High for this recovery window (mirrors the reclaim
/// coordinator's stability hysteresis).
const RECOVERY_STABILITY_MS: u64 = 30_000;

/// Per-pass bound on drained connections (L4 and H3 each).
const SHED_BATCH_LIMIT: usize = 32;

/// Keepalive suppression flag, read once per request on the proxy hot path.
static KEEPALIVE_SHED_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Monotonic ms when the current unbroken Critical episode began
/// (`u64::MAX` = no episode).
static CRITICAL_SINCE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Monotonic ms of the most recent *fresh* Critical sample.
static LAST_CRITICAL_SAMPLE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Fresh (time-separated) Critical samples in the current episode.
static CRITICAL_SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Monotonic ms of the latest ineffective reclaim pass (`u64::MAX` = none).
static INEFFECTIVE_RECLAIM_AT_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Monotonic ms of the last final-resort drain pass (`u64::MAX` = none).
static LAST_FINAL_RESORT_AT_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Final-resort drain passes taken, cumulative.
static FINAL_RESORT_PASSES: AtomicU64 = AtomicU64::new(0);

/// Monotonic ms when the observed level first dropped below High
/// (u64::MAX = not recovering).
static RECOVERY_SINCE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

/// Connections cancelled by shed passes, cumulative.
static SHED_DRAINED_CONNECTIONS: AtomicU64 = AtomicU64::new(0);

/// Requests that ran with keepalive suppressed under shed, cumulative.
static SHED_KEEPALIVE_MARKED: AtomicU64 = AtomicU64::new(0);

/// Hot-path read: should responses suppress keepalive right now?
/// One relaxed load; false is the overwhelmingly common answer.
#[inline]
pub fn shed_keepalive_active() -> bool {
    KEEPALIVE_SHED_ACTIVE.load(Ordering::Relaxed)
}

/// Metric hook: a response was written with keepalive suppressed.
#[inline]
pub fn note_keepalive_shed() {
    SHED_KEEPALIVE_MARKED.fetch_add(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ShedStats {
    pub keepalive_shed_active: bool,
    pub critical_samples: u64,
    pub drained_connections_total: u64,
    pub keepalive_marked_total: u64,
    /// The latest reclaim pass was measurably ineffective and its evidence
    /// is still fresh enough to justify a final-resort pass.
    pub reclaim_exhausted: bool,
    pub final_resort_passes_total: u64,
    pub last_final_resort_at_ms: Option<u64>,
}

pub fn shed_stats() -> ShedStats {
    let now = monotonic_now_ms();
    let evidence_at = INEFFECTIVE_RECLAIM_AT_MS.load(Ordering::Relaxed);
    let last_pass = LAST_FINAL_RESORT_AT_MS.load(Ordering::Relaxed);
    ShedStats {
        keepalive_shed_active: shed_keepalive_active(),
        critical_samples: CRITICAL_SAMPLES.load(Ordering::Relaxed),
        drained_connections_total: SHED_DRAINED_CONNECTIONS.load(Ordering::Relaxed),
        keepalive_marked_total: SHED_KEEPALIVE_MARKED.load(Ordering::Relaxed),
        reclaim_exhausted: ineffective_evidence_fresh(now, evidence_at),
        final_resort_passes_total: FINAL_RESORT_PASSES.load(Ordering::Relaxed),
        last_final_resort_at_ms: (last_pass != u64::MAX).then_some(last_pass),
    }
}

/// Called from `memory_reclaim::on_memory_pressure_observed` with the raw
/// observed level on every pressure sample (periodic check, PSI watcher,
/// hot-path notifications). Sub-Critical samples cost a few atomic stores;
/// only a fully gated final-resort sample reads the governor snapshot.
pub fn observe_pressure(level: MemoryPressureLevel) {
    observe_pressure_at(level, monotonic_now_ms());
}

/// Record the outcome of a reclaim pass that actually ran. Called by
/// `memory_reclaim` after the pass completes and after the coordinator lock
/// is released, so shed never waits on the reclaim coordinator.
pub fn note_reclaim_outcome(ineffective: bool, now_ms: u64) {
    if ineffective {
        INEFFECTIVE_RECLAIM_AT_MS.store(now_ms, Ordering::Release);
    } else {
        // A reclaim that measurably helped breaks the exhaustion chain: only
        // a *later* ineffective pass may justify shedding again.
        INEFFECTIVE_RECLAIM_AT_MS.store(u64::MAX, Ordering::Release);
    }
}

/// Monotonic clock shared with `memory_reclaim` so evidence timestamps and
/// observation timestamps use the same epoch.
fn monotonic_now_ms() -> u64 {
    crate::memory_reclaim::monotonic_now_ms()
}

/// Account state the final-resort predicate needs. Kept as explicit inputs
/// so the predicate is deterministic under test instead of depending on the
/// live governor.
#[derive(Clone, Copy, Debug, Default)]
struct AccountPressureInputs {
    headroom_bytes: Option<u64>,
    connection_admission_used_bytes: u64,
    request_workspace_used_bytes: u64,
    actual_pressure: MemoryPressureLevel,
}

/// Read one coherent, refreshed governor snapshot. Only called once the
/// sustained-Critical and ineffective-reclaim gates have passed, so the
/// (comparatively heavy) snapshot refresh never runs on ordinary samples.
/// Using the snapshot's own pressure level also rejects a stale or PSI-only
/// `Critical` that the actual memory observation does not confirm.
fn current_account_inputs() -> AccountPressureInputs {
    let governor = &crate::memory_governor::MEMORY_GOVERNOR;
    let snapshot = governor.snapshot(governor.pingora_worker_threads());
    AccountPressureInputs {
        headroom_bytes: snapshot.account_headroom_bytes,
        connection_admission_used_bytes: snapshot.connection_admission_used_bytes,
        request_workspace_used_bytes: snapshot.request_workspace_used_bytes,
        actual_pressure: snapshot.memory_pressure_level,
    }
}

fn observe_pressure_at(level: MemoryPressureLevel, now_ms: u64) {
    observe_pressure_with_source(level, now_ms, current_account_inputs);
}

/// Time-driven core of `observe_pressure`. The account source is a closure so
/// the snapshot is only read after the timing and evidence gates pass, while
/// tests can inject deterministic inputs.
fn observe_pressure_with_source<F>(level: MemoryPressureLevel, now_ms: u64, account: F)
where
    F: FnOnce() -> AccountPressureInputs,
{
    match level {
        MemoryPressureLevel::High => {
            KEEPALIVE_SHED_ACTIVE.store(true, Ordering::Relaxed);
            reset_critical_episode();
            // A High sample starts a new Critical episode: the old episode's
            // exhaustion evidence does not carry over.
            INEFFECTIVE_RECLAIM_AT_MS.store(u64::MAX, Ordering::Relaxed);
            RECOVERY_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
        }
        MemoryPressureLevel::Critical => {
            KEEPALIVE_SHED_ACTIVE.store(true, Ordering::Relaxed);
            RECOVERY_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
            if !record_fresh_critical_sample(now_ms) {
                return;
            }
            let since = CRITICAL_SINCE_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(since) < CRITICAL_SUSTAIN_MS
                || CRITICAL_SAMPLES.load(Ordering::Relaxed) < 2
            {
                return;
            }
            let evidence_at = INEFFECTIVE_RECLAIM_AT_MS.load(Ordering::Relaxed);
            if !ineffective_evidence_fresh(now_ms, evidence_at) {
                return;
            }
            let last_pass = LAST_FINAL_RESORT_AT_MS.load(Ordering::Relaxed);
            if last_pass != u64::MAX
                && now_ms.saturating_sub(last_pass) < FINAL_RESORT_MIN_INTERVAL_MS
            {
                return;
            }
            let account = account();
            let facts = FinalResortFacts {
                now_ms,
                actual_pressure: account.actual_pressure,
                headroom_bytes: account.headroom_bytes,
                working_set_bytes: account
                    .connection_admission_used_bytes
                    .saturating_add(account.request_workspace_used_bytes),
                ineffective_reclaim_at_ms: evidence_at,
                last_pass_at_ms: last_pass,
            };
            if !final_resort_allowed(facts) {
                return;
            }
            // Claim the pass slot atomically. `observe_pressure` runs on the
            // reclaim monitor but can also be reached from the cache
            // janitor's periodic check, so two evaluations may overlap; the
            // claim must still uphold the 30s floor across threads.
            let mut last = last_pass;
            loop {
                if last != u64::MAX && now_ms.saturating_sub(last) < FINAL_RESORT_MIN_INTERVAL_MS {
                    return;
                }
                match LAST_FINAL_RESORT_AT_MS.compare_exchange_weak(
                    last,
                    now_ms,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => last = observed,
                }
            }
            // Consume the evidence so one ineffective reclaim can justify at
            // most one pass, even if the registries turn out to be empty.
            INEFFECTIVE_RECLAIM_AT_MS.store(u64::MAX, Ordering::Relaxed);
            FINAL_RESORT_PASSES.fetch_add(1, Ordering::Relaxed);
            let l4 = l4_connection_registry::L4_CONNECTION_REGISTRY.drain_oldest_with_reason(
                SHED_BATCH_LIMIT,
                ConnectionCancelReason::MemoryPressureShed,
            );
            let h3 = crate::http3_proxy_manager::drain_h3_oldest(SHED_BATCH_LIMIT);
            let drained = l4 + h3;
            if drained > 0 {
                SHED_DRAINED_CONNECTIONS.fetch_add(drained as u64, Ordering::Relaxed);
            }
            tracing::warn!(
                target: "memory_shed",
                l4,
                h3,
                working_set_bytes = facts.working_set_bytes,
                headroom_bytes = facts.headroom_bytes,
                critical_samples = CRITICAL_SAMPLES.load(Ordering::Relaxed),
                "final-resort memory shed: reclaim exhausted, drained oldest in-flight connections"
            );
        }
        _ => {
            // Sub-High: the Critical episode is broken immediately; the
            // suppression flag lifts and the counters reset only once the
            // level has held below High for the full recovery window — a
            // single quiet sample mid-incident must not de-escalate.
            reset_critical_episode();
            let since = match RECOVERY_SINCE_MS.compare_exchange(
                u64::MAX,
                now_ms,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => now_ms,
                Err(existing) => existing,
            };
            if now_ms.saturating_sub(since) >= RECOVERY_STABILITY_MS {
                KEEPALIVE_SHED_ACTIVE.store(false, Ordering::Relaxed);
                RECOVERY_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
                // Stable recovery retires the exhaustion evidence too: a
                // later incident must earn fresh ineffective-reclaim proof.
                INEFFECTIVE_RECLAIM_AT_MS.store(u64::MAX, Ordering::Relaxed);
            }
        }
    }
}

/// Record a Critical sample if it is at least `CRITICAL_SAMPLE_MIN_INTERVAL_MS`
/// after the previous fresh one. Returns false for a too-close repeat, which
/// must not extend or advance sustained evidence.
fn record_fresh_critical_sample(now_ms: u64) -> bool {
    let last = LAST_CRITICAL_SAMPLE_MS.load(Ordering::Relaxed);
    if last == u64::MAX {
        CRITICAL_SINCE_MS.store(now_ms, Ordering::Relaxed);
    } else if now_ms.saturating_sub(last) < CRITICAL_SAMPLE_MIN_INTERVAL_MS {
        return false;
    }
    LAST_CRITICAL_SAMPLE_MS.store(now_ms, Ordering::Relaxed);
    CRITICAL_SAMPLES.fetch_add(1, Ordering::Relaxed);
    true
}

fn reset_critical_episode() {
    CRITICAL_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
    LAST_CRITICAL_SAMPLE_MS.store(u64::MAX, Ordering::Relaxed);
    CRITICAL_SAMPLES.store(0, Ordering::Relaxed);
}

fn ineffective_evidence_fresh(now_ms: u64, evidence_at_ms: u64) -> bool {
    evidence_at_ms != u64::MAX
        && now_ms.saturating_sub(evidence_at_ms) <= INEFFECTIVE_EVIDENCE_MAX_AGE_MS
}

/// Inputs to the final-resort predicate, kept separate from the atomics so
/// boundary behaviour is unit-testable.
#[derive(Clone, Copy, Debug)]
struct FinalResortFacts {
    now_ms: u64,
    actual_pressure: MemoryPressureLevel,
    headroom_bytes: Option<u64>,
    working_set_bytes: u64,
    /// `u64::MAX` = no ineffective evidence.
    ineffective_reclaim_at_ms: u64,
    /// `u64::MAX` = no pass yet.
    last_pass_at_ms: u64,
}

/// The complete, deliberately narrow final-resort predicate:
///
/// * `Critical` must be confirmed by the actual memory snapshot, not just a
///   PSI/cgroup-event floor;
/// * headroom must be *known* and strictly below the working set;
/// * ineffective reclaim evidence must be present and no older than 30s;
/// * the previous pass must be at least 30s old.
fn final_resort_allowed(facts: FinalResortFacts) -> bool {
    if facts.actual_pressure != MemoryPressureLevel::Critical {
        return false;
    }
    let Some(headroom) = facts.headroom_bytes else {
        return false;
    };
    if headroom >= facts.working_set_bytes {
        return false;
    }
    if !ineffective_evidence_fresh(facts.now_ms, facts.ineffective_reclaim_at_ms) {
        return false;
    }
    if facts.last_pass_at_ms != u64::MAX
        && facts.now_ms.saturating_sub(facts.last_pass_at_ms) < FINAL_RESORT_MIN_INTERVAL_MS
    {
        return false;
    }
    true
}

#[cfg(test)]
fn observe_pressure_with(level: MemoryPressureLevel, now_ms: u64, account: AccountPressureInputs) {
    observe_pressure_with_source(level, now_ms, || account);
}

#[cfg(test)]
fn reset_test_state() {
    KEEPALIVE_SHED_ACTIVE.store(false, Ordering::Relaxed);
    CRITICAL_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
    LAST_CRITICAL_SAMPLE_MS.store(u64::MAX, Ordering::Relaxed);
    CRITICAL_SAMPLES.store(0, Ordering::Relaxed);
    INEFFECTIVE_RECLAIM_AT_MS.store(u64::MAX, Ordering::Relaxed);
    LAST_FINAL_RESORT_AT_MS.store(u64::MAX, Ordering::Relaxed);
    FINAL_RESORT_PASSES.store(0, Ordering::Relaxed);
    RECOVERY_SINCE_MS.store(u64::MAX, Ordering::Relaxed);
    SHED_DRAINED_CONNECTIONS.store(0, Ordering::Relaxed);
    SHED_KEEPALIVE_MARKED.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l4_connection_registry::{self, L4ConnectionProtocol};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(last: u8, port_offset: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(198, 51, 100, last)),
            41000 + port_offset,
        )
    }

    fn account(
        headroom: Option<u64>,
        admission_used: u64,
        workspace_used: u64,
    ) -> AccountPressureInputs {
        AccountPressureInputs {
            headroom_bytes: headroom,
            connection_admission_used_bytes: admission_used,
            request_workspace_used_bytes: workspace_used,
            actual_pressure: MemoryPressureLevel::Critical,
        }
    }

    fn tight_account() -> AccountPressureInputs {
        account(Some(100), 200, 0)
    }

    fn ample_account() -> AccountPressureInputs {
        account(Some(10_000), 200, 0)
    }

    /// Three fresh samples 5s apart: sustain reaches exactly 10s.
    fn drive_sustained_critical(start_ms: u64, account: AccountPressureInputs) {
        observe_pressure_with(MemoryPressureLevel::Critical, start_ms, account);
        observe_pressure_with(MemoryPressureLevel::Critical, start_ms + 5_000, account);
        observe_pressure_with(MemoryPressureLevel::Critical, start_ms + 10_000, account);
    }

    #[test]
    fn high_pressure_suppresses_keepalive_without_draining() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        let guard = l4_connection_registry::register(addr(1, 1), L4ConnectionProtocol::Http1);
        let mut rx = guard.cancel_receiver();

        observe_pressure_with(
            MemoryPressureLevel::High,
            1_000_100,
            AccountPressureInputs::default(),
        );
        assert!(shed_keepalive_active());
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "High must not cancel in-flight connections"
        );
        // Sub-High within the recovery window keeps suppression active;
        // only a stable 30s below High lifts it.
        observe_pressure_with(
            MemoryPressureLevel::Normal,
            1_001_000,
            AccountPressureInputs::default(),
        );
        assert!(shed_keepalive_active());
        observe_pressure_with(
            MemoryPressureLevel::Normal,
            1_031_000,
            AccountPressureInputs::default(),
        );
        assert!(!shed_keepalive_active());
        drop(guard);
    }

    #[test]
    fn critical_alone_never_drains_without_reclaim_exhaustion() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        let guard = l4_connection_registry::register(addr(2, 2), L4ConnectionProtocol::Http1);
        let mut rx = guard.cancel_receiver();

        // Sustained Critical with actual low headroom, but no ineffective
        // reclaim evidence: the account pressure alone must not kill.
        drive_sustained_critical(2_000_000, tight_account());
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "Critical without an ineffective reclaim must not shed"
        );
        let stats = shed_stats();
        assert_eq!(stats.final_resort_passes_total, 0);
        assert_eq!(stats.drained_connections_total, 0);
        assert_eq!(stats.critical_samples, 3);
        drop(guard);
    }

    #[test]
    fn progressing_reclaim_never_drains() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        let guard = l4_connection_registry::register(addr(3, 3), L4ConnectionProtocol::Http2);
        let mut rx = guard.cancel_receiver();

        // A measurably effective reclaim clears stale evidence.
        note_reclaim_outcome(true, 3_000_000);
        note_reclaim_outcome(false, 3_000_100);
        drive_sustained_critical(3_000_200, tight_account());
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "a progressing reclaim must not be followed by a kill"
        );
        assert_eq!(shed_stats().final_resort_passes_total, 0);
        drop(guard);
    }

    #[test]
    fn final_resort_predicate_boundaries() {
        let base = FinalResortFacts {
            now_ms: 100_000,
            actual_pressure: MemoryPressureLevel::Critical,
            headroom_bytes: Some(99),
            working_set_bytes: 100,
            ineffective_reclaim_at_ms: 70_000, // exactly 30s old
            last_pass_at_ms: u64::MAX,
        };
        assert!(
            final_resort_allowed(base),
            "headroom strictly below the working set with fresh evidence is allowed"
        );

        // Headroom boundary: equal is not "below".
        assert!(!final_resort_allowed(FinalResortFacts {
            headroom_bytes: Some(100),
            ..base
        }));
        assert!(!final_resort_allowed(FinalResortFacts {
            headroom_bytes: Some(101),
            ..base
        }));
        assert!(
            final_resort_allowed(FinalResortFacts {
                headroom_bytes: Some(0),
                working_set_bytes: 1,
                ..base
            }),
            "zero headroom below a nonzero working set is allowed"
        );
        assert!(
            !final_resort_allowed(FinalResortFacts {
                headroom_bytes: Some(0),
                working_set_bytes: 0,
                ..base
            }),
            "an empty working set has nothing to reclaim"
        );

        // Unknown account data must never kill.
        assert!(!final_resort_allowed(FinalResortFacts {
            headroom_bytes: None,
            ..base
        }));

        // A PSI-only escalation the actual snapshot does not confirm is not
        // enough on its own.
        assert!(!final_resort_allowed(FinalResortFacts {
            actual_pressure: MemoryPressureLevel::High,
            ..base
        }));

        // Stale or absent reclaim evidence.
        assert!(!final_resort_allowed(FinalResortFacts {
            ineffective_reclaim_at_ms: u64::MAX,
            ..base
        }));
        assert!(final_resort_allowed(FinalResortFacts {
            ineffective_reclaim_at_ms: 70_000,
            ..base
        }));
        assert!(
            !final_resort_allowed(FinalResortFacts {
                ineffective_reclaim_at_ms: 69_999, // 30_001ms old
                ..base
            }),
            "evidence older than 30s must not authorize a pass"
        );

        // Cooldown boundary.
        assert!(!final_resort_allowed(FinalResortFacts {
            last_pass_at_ms: 70_001, // 29_999ms ago
            ..base
        }));
        assert!(
            final_resort_allowed(FinalResortFacts {
                last_pass_at_ms: 70_000, // exactly 30s ago
                ..base
            }),
            "30s since the previous pass is enough"
        );

        // No pass has ever run: the cooldown does not suppress the first one.
        assert!(final_resort_allowed(FinalResortFacts {
            last_pass_at_ms: u64::MAX,
            ..base
        }));
    }

    #[test]
    fn stale_snapshot_repeats_do_not_build_sustained_critical() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        note_reclaim_outcome(true, 4_000_000);
        let guard = l4_connection_registry::register(addr(4, 4), L4ConnectionProtocol::Http1);
        let mut rx = guard.cancel_receiver();

        observe_pressure_with(MemoryPressureLevel::Critical, 4_000_000, tight_account());
        // All repeats are inside the 5s sample interval; even though several
        // seconds pass, none is a fresh sample, so the episode never matures.
        for step in 1..=9u64 {
            observe_pressure_with(
                MemoryPressureLevel::Critical,
                4_000_000 + step * 500,
                tight_account(),
            );
        }
        assert_eq!(
            shed_stats().critical_samples,
            1,
            "repeats inside the sample interval must not count as fresh evidence"
        );
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "stale-sample repeats must not authorize a drain"
        );
        drop(guard);
    }

    #[test]
    fn sustained_critical_with_ineffective_reclaim_drains_then_consumes_evidence() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        note_reclaim_outcome(true, 5_000_000);
        let guard = l4_connection_registry::register(addr(5, 5), L4ConnectionProtocol::Http1);
        let mut rx = guard.cancel_receiver();

        drive_sustained_critical(5_000_000, tight_account());
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::MemoryPressureShed,
            "all final-resort gates met must drain the oldest connection"
        );
        let stats = shed_stats();
        assert_eq!(stats.final_resort_passes_total, 1);
        assert!(
            !stats.reclaim_exhausted,
            "the pass must consume the ineffective-reclaim evidence"
        );
        assert_eq!(stats.last_final_resort_at_ms, Some(5_010_000));
        drop(guard);
    }

    #[test]
    fn final_resort_is_rate_limited_to_one_pass_per_thirty_seconds() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        note_reclaim_outcome(true, 6_000_000);
        let first = l4_connection_registry::register(addr(6, 6), L4ConnectionProtocol::Http1);
        let mut first_rx = first.cancel_receiver();
        drive_sustained_critical(6_000_000, tight_account());
        assert_eq!(
            *first_rx.borrow_and_update(),
            ConnectionCancelReason::MemoryPressureShed
        );

        // Fresh evidence again, but only 5s after the pass: the cooldown
        // must hold even though the sample is fresh.
        let second = l4_connection_registry::register(addr(6, 7), L4ConnectionProtocol::Http1);
        let mut second_rx = second.cancel_receiver();
        note_reclaim_outcome(true, 6_015_000);
        observe_pressure_with(MemoryPressureLevel::Critical, 6_015_000, tight_account());
        assert_eq!(
            *second_rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "a second pass inside 30s must not run"
        );
        assert_eq!(shed_stats().final_resort_passes_total, 1);

        // 30s after the pass with fresh evidence: allowed again.
        note_reclaim_outcome(true, 6_040_000);
        observe_pressure_with(MemoryPressureLevel::Critical, 6_040_000, tight_account());
        assert_eq!(
            *second_rx.borrow_and_update(),
            ConnectionCancelReason::MemoryPressureShed,
            "the next pass is allowed once the 30s interval has elapsed"
        );
        assert_eq!(shed_stats().final_resort_passes_total, 2);
        drop(first);
        drop(second);
    }

    #[test]
    fn deescalation_resets_critical_episode_and_lifts_suppression() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        note_reclaim_outcome(true, 7_000_000);
        let guard = l4_connection_registry::register(addr(7, 8), L4ConnectionProtocol::Http2);
        let mut rx = guard.cancel_receiver();

        observe_pressure_with(MemoryPressureLevel::Critical, 7_000_000, ample_account());
        observe_pressure_with(MemoryPressureLevel::Critical, 7_005_000, ample_account());
        assert_eq!(shed_stats().critical_samples, 2);
        assert!(shed_keepalive_active());

        // First sub-High sample breaks the sustained episode immediately,
        // but suppression survives the recovery window.
        observe_pressure_with(MemoryPressureLevel::Normal, 7_006_000, ample_account());
        assert_eq!(shed_stats().critical_samples, 0);
        assert!(shed_keepalive_active());
        observe_pressure_with(MemoryPressureLevel::Normal, 7_036_000, ample_account());
        assert!(!shed_keepalive_active());
        assert_eq!(
            *rx.borrow_and_update(),
            ConnectionCancelReason::None,
            "deescalation must never drain"
        );
        assert_eq!(shed_stats().final_resort_passes_total, 0);
        drop(guard);
    }

    #[test]
    fn stats_reflect_activity() {
        let _serial = l4_connection_registry::REGISTRY_TEST_LOCK.lock().unwrap();
        reset_test_state();
        observe_pressure_with(
            MemoryPressureLevel::High,
            8_000_100,
            AccountPressureInputs::default(),
        );
        note_keepalive_shed();
        let stats = shed_stats();
        assert!(stats.keepalive_shed_active);
        assert!(stats.keepalive_marked_total >= 1);
        // First sub-High sample opens the recovery window; the next
        // sample ≥30s later lifts suppression.
        observe_pressure_with(
            MemoryPressureLevel::Normal,
            8_001_000,
            AccountPressureInputs::default(),
        );
        observe_pressure_with(
            MemoryPressureLevel::Normal,
            8_032_000,
            AccountPressureInputs::default(),
        );
        assert!(!shed_stats().keepalive_shed_active);
    }
}
