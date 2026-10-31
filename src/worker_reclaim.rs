//! Deterministic mimalloc heap collection on self-owned Tokio worker threads.
//!
//! `mi_collect` only purges the calling thread's mimalloc heap. The cache
//! janitor used to rely on its Tokio task migrating between runtime workers
//! ("spawn lottery") so that, over time, different heaps were visited. That is
//! neither deterministic nor complete: a task may run on the same worker
//! forever, and a busy worker may never be visited at all. Tokio's
//! `on_thread_start` / `on_thread_park` / `on_thread_stop` callbacks run on the
//! actual worker threads, so each heap is collected at its own bounded pace
//! without putting collection work on the request path.
//!
//! # Coverage boundary
//!
//! These hooks are installed only on the runtime this process builds itself in
//! `main` (`tokio::runtime::Builder::new_multi_thread`), including the custom
//! HTTP/TCP/UDP accept loops. Pingora builds its separate control/background
//! runtimes internally; those threads are not covered. Tokio blocking-pool
//! threads run start/stop hooks but never the park hook; durable storage work
//! therefore uses [`spawn_storage_work`] to collect on completion. A worker
//! that stays parked has no periodic callback until it wakes and parks again.
//! [`WorkerReclaimSnapshot`] therefore reports the hooked worker population
//! and the configured-but-unhooked Pingora population separately;
//! [`WorkerReclaimSnapshot::covers_all_workers`] stays false until an actual
//! hook exists for every worker. Never describe partial coverage as full
//! coverage.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Hard floor between two park-time collects on the same worker. A pressure
/// generation bump can ask for a collect, but it cannot make a worker run
/// `mi_collect` more often than this; the request is satisfied at the next
/// park after the interval, which bounds maintenance CPU per worker.
const PARK_COLLECT_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// A worker that parks again after this interval collects even without a new
/// generation request. A continuously parked worker does not run this check.
const PARK_COLLECT_IDLE_INTERVAL: Duration = Duration::from_secs(30);

static COLLECT_GENERATION: AtomicU64 = AtomicU64::new(1);
static COLLECT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_STARTED: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_LIVE: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_STOPPED: AtomicU64 = AtomicU64::new(0);
static PARK_COLLECTS: AtomicU64 = AtomicU64::new(0);
static PARK_COLLECTS_RATE_LIMITED: AtomicU64 = AtomicU64::new(0);
static STOP_COLLECTS: AtomicU64 = AtomicU64::new(0);
static UNHOOKED_PINGORA_WORKERS: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct WorkerCollectState {
    /// Generation observed by this worker at its last collect attempt.
    generation: u64,
    last_collect: Option<Instant>,
}

thread_local! {
    static WORKER_COLLECT_STATE: Cell<WorkerCollectState> = const {
        Cell::new(WorkerCollectState {
            generation: 0,
            last_collect: None,
        })
    };
}

/// Observable worker-collect progress and the coverage boundary.
///
/// `worker_threads_live` counts threads currently executing the installed
/// start/park/stop hooks. `unhooked_pingora_workers` is configured by `main`
/// from Pingora's `ServerConf::threads`. Pingora may create several service
/// runtimes with that per-runtime count, so the reported figure is a lower
/// bound; any nonzero value keeps [`Self::covers_all_workers`] false, which is
/// the accurate boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerReclaimSnapshot {
    pub collect_generation: u64,
    pub collect_requests: u64,
    pub worker_threads_started: u64,
    pub worker_threads_live: u64,
    pub worker_threads_stopped: u64,
    pub park_collects: u64,
    pub park_collects_rate_limited: u64,
    pub stop_collects: u64,
    pub unhooked_pingora_workers: u64,
}

impl WorkerReclaimSnapshot {
    /// True only when every worker represented in this snapshot is covered:
    /// at least one hooked worker is live and no configured Pingora worker is
    /// unhooked. Blocking-pool threads are represented through their
    /// start/stop hooks (they have no park hook); see the module coverage
    /// note.
    pub fn covers_all_workers(&self) -> bool {
        self.worker_threads_live > 0 && self.unhooked_pingora_workers == 0
    }
}

pub fn snapshot() -> WorkerReclaimSnapshot {
    WorkerReclaimSnapshot {
        collect_generation: COLLECT_GENERATION.load(Ordering::Acquire),
        collect_requests: COLLECT_REQUESTS.load(Ordering::Relaxed),
        worker_threads_started: WORKER_THREADS_STARTED.load(Ordering::Relaxed),
        worker_threads_live: WORKER_THREADS_LIVE.load(Ordering::Relaxed),
        worker_threads_stopped: WORKER_THREADS_STOPPED.load(Ordering::Relaxed),
        park_collects: PARK_COLLECTS.load(Ordering::Relaxed),
        park_collects_rate_limited: PARK_COLLECTS_RATE_LIMITED.load(Ordering::Relaxed),
        stop_collects: STOP_COLLECTS.load(Ordering::Relaxed),
        unhooked_pingora_workers: UNHOOKED_PINGORA_WORKERS.load(Ordering::Relaxed),
    }
}

/// Install start/park/stop hooks on a Tokio runtime builder.
///
/// Call this only for runtimes this process owns. The hooks are per worker
/// thread and idempotent at the builder level (Tokio keeps one callback per
/// event), so calling this on the single main runtime is the intended use.
pub fn install_tokio_worker_hooks(builder: &mut tokio::runtime::Builder) {
    builder
        .on_thread_start(on_worker_start)
        .on_thread_park(on_worker_park)
        .on_thread_stop(on_worker_stop);
}

/// Run durable storage work on Tokio's blocking pool and collect its freed
/// pages before that thread returns to the pool. Blocking threads never run
/// `on_thread_park`; start/stop hooks alone leave cross-thread frees pending
/// while the pool keeps the allocating thread alive.
pub(crate) fn spawn_storage_work<F, T>(work: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        struct CollectOnCompletion;
        impl Drop for CollectOnCompletion {
            fn drop(&mut self) {
                crate::memory_reclaim::trim_expired_heap_pages();
            }
        }
        let _collect = CollectOnCompletion;
        work()
    })
}

/// Record the configured Pingora service-worker population that has no
/// collect hook.
///
/// Pingora's runtimes are built inside the vendored crate; this process cannot
/// install Tokio callbacks on them. Each service runtime uses
/// `ServerConf::threads` unless the service overrides it, so the reported
/// value is a lower bound. Reporting it keeps the coverage boundary
/// observable instead of silently claiming all workers are covered.
pub fn set_unhooked_pingora_workers(count: usize) {
    UNHOOKED_PINGORA_WORKERS.store(count as u64, Ordering::Relaxed);
}

/// Ask every hooked worker to collect its heap at its next park.
///
/// The generation is the coalescing signal: repeated requests before a worker
/// parks collapse into one collect, and the per-worker interval still applies.
pub fn request_worker_collect() {
    COLLECT_REQUESTS.fetch_add(1, Ordering::Relaxed);
    COLLECT_GENERATION.fetch_add(1, Ordering::AcqRel);
}

enum ParkDecision {
    Collect,
    RateLimited,
    NotDue,
}

fn on_worker_start() {
    WORKER_THREADS_STARTED.fetch_add(1, Ordering::Relaxed);
    WORKER_THREADS_LIVE.fetch_add(1, Ordering::Relaxed);
    // `last_collect: None` makes the first park collect once, so a freshly
    // started worker does not inherit a stale generation verdict.
    WORKER_COLLECT_STATE.with(|state| {
        state.set(WorkerCollectState {
            generation: COLLECT_GENERATION.load(Ordering::Acquire),
            last_collect: None,
        })
    });
}

fn on_worker_park() {
    let now = Instant::now();
    let generation = COLLECT_GENERATION.load(Ordering::Acquire);
    let decision = WORKER_COLLECT_STATE.with(|cell| {
        let state = cell.get();
        let generation_due = state.generation != generation;
        let idle_due = state
            .last_collect
            .is_none_or(|last| now.duration_since(last) >= PARK_COLLECT_IDLE_INTERVAL);
        if !generation_due && !idle_due {
            return ParkDecision::NotDue;
        }
        let interval_ok = state
            .last_collect
            .is_none_or(|last| now.duration_since(last) >= PARK_COLLECT_MIN_INTERVAL);
        if !interval_ok {
            return ParkDecision::RateLimited;
        }
        cell.set(WorkerCollectState {
            generation,
            last_collect: Some(now),
        });
        ParkDecision::Collect
    });

    match decision {
        ParkDecision::Collect => {
            // Non-forced: purge only pages whose retention expired. This is
            // the bounded maintenance collect, safe to repeat while parked.
            crate::memory_reclaim::trim_expired_heap_pages();
            PARK_COLLECTS.fetch_add(1, Ordering::Relaxed);
        }
        ParkDecision::RateLimited => {
            PARK_COLLECTS_RATE_LIMITED.fetch_add(1, Ordering::Relaxed);
        }
        ParkDecision::NotDue => {}
    }
}

fn on_worker_stop() {
    // The worker is going away and its heap is the only place these freed
    // pages can be returned from; a forced collect here is bounded to one
    // heap and cannot affect other workers.
    crate::memory_reclaim::trim_released_heap();
    STOP_COLLECTS.fetch_add(1, Ordering::Relaxed);
    WORKER_THREADS_LIVE.fetch_sub(1, Ordering::Relaxed);
    WORKER_THREADS_STOPPED.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// The counters, the unhooked-worker gauge and the cache janitor's
    /// negative-cache state are process-global. Tests that assert deltas must
    /// not interleave with each other or with cache tests.
    fn lock_tests() -> tokio::sync::MutexGuard<'static, ()> {
        crate::cache_hybrid::CACHE_GLOBAL_STATE_LOCK.blocking_lock()
    }

    /// Clear this thread's rate-limit state so a test can observe the next
    /// generation without waiting out `PARK_COLLECT_MIN_INTERVAL`.
    fn reset_park_interval_for_test() {
        WORKER_COLLECT_STATE.with(|cell| {
            let mut state = cell.get();
            state.last_collect = None;
            cell.set(state);
        });
    }

    #[repr(C)]
    struct AllocatorAreaPrefix {
        blocks: *mut std::ffi::c_void,
        reserved: usize,
        committed: usize,
        used: usize,
        block_size: usize,
    }

    unsafe extern "C" {
        fn mi_theap_get_default() -> *mut std::ffi::c_void;
        fn mi_theap_visit_blocks(
            theap: *mut std::ffi::c_void,
            visit_blocks: bool,
            visitor: unsafe extern "C" fn(
                *const std::ffi::c_void,
                *const AllocatorAreaPrefix,
                *mut std::ffi::c_void,
                usize,
                *mut std::ffi::c_void,
            ) -> bool,
            arg: *mut std::ffi::c_void,
        ) -> bool;
    }

    fn large_thread_owned_bytes() -> usize {
        // Read only the stable public mi_heap_area_t prefix, without visiting
        // allocation contents or collecting the deferred free list first.
        unsafe extern "C" fn count(
            _heap: *const std::ffi::c_void,
            area: *const AllocatorAreaPrefix,
            _block: *mut std::ffi::c_void,
            _block_size: usize,
            arg: *mut std::ffi::c_void,
        ) -> bool {
            let area = unsafe { &*area };
            if area.block_size >= 96 * 1024 {
                unsafe { *(arg as *mut usize) += area.used * area.block_size };
            }
            true
        }
        let mut bytes = 0;
        unsafe {
            // The allocator also returns false for an empty thread heap.
            mi_theap_visit_blocks(
                mi_theap_get_default(),
                false,
                count,
                (&mut bytes as *mut usize).cast(),
            );
        }
        bytes
    }

    #[test]
    fn storage_completion_collects_cross_thread_frees_on_success_error_and_panic() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("storage test runtime");
        for outcome in 0..3 {
            let (blocks_tx, blocks_rx) = mpsc::channel::<Vec<usize>>();
            let (freed_tx, freed_rx) = mpsc::channel();
            let free_thread = std::thread::spawn(move || {
                for block in blocks_rx.recv().expect("allocated blocks") {
                    unsafe { libmimalloc_sys::mi_free(block as *mut std::ffi::c_void) };
                }
                freed_tx.send(()).expect("signal cross-thread free");
            });
            runtime.block_on(async {
                let task = spawn_storage_work(move || {
                    let blocks = (0..8)
                        .map(|index| {
                            let size = (96 + index * 32) * 1024;
                            let block = unsafe { libmimalloc_sys::mi_malloc_aligned(size, 8) };
                            assert!(!block.is_null());
                            unsafe { std::ptr::write_bytes(block.cast::<u8>(), 7, size) };
                            block as usize
                        })
                        .collect();
                    blocks_tx.send(blocks).expect("send allocations");
                    freed_rx.recv().expect("cross-thread frees completed");
                    assert!(large_thread_owned_bytes() >= 1024 * 1024);
                    match outcome {
                        0 => Ok(7),
                        1 => Err("durable write failed"),
                        _ => panic!("injected storage panic"),
                    }
                });
                match outcome {
                    0 => assert_eq!(task.await.unwrap(), Ok(7)),
                    1 => assert_eq!(task.await.unwrap(), Err("durable write failed")),
                    _ => assert!(task.await.unwrap_err().is_panic()),
                }
                // One blocking thread forces the probe onto the same live
                // heap. A raw task must inspect it without invoking the fix.
                let retained = tokio::task::spawn_blocking(large_thread_owned_bytes)
                    .await
                    .unwrap();
                assert_eq!(
                    retained, 0,
                    "completed storage task retained deferred frees"
                );
            });
            free_thread.join().expect("freeing thread");
        }
    }

    #[test]
    fn park_collects_once_then_rate_limits_until_interval_or_generation() {
        let _guard = lock_tests();
        let before = snapshot();
        let handle = std::thread::spawn(move || {
            on_worker_start();
            // First park on a fresh thread is due (never collected).
            on_worker_park();
            let after_first = snapshot();
            assert_eq!(
                after_first.park_collects,
                before.park_collects + 1,
                "fresh worker must collect its heap on the first park"
            );

            // A second park immediately is not due at all: same generation,
            // idle interval not reached.
            on_worker_park();
            assert_eq!(
                snapshot().park_collects,
                after_first.park_collects,
                "repeat park must not collect inside the rate-limit window"
            );

            // A generation request makes it due, but the interval still
            // applies, so the request is rate limited rather than queued as
            // an unbounded collect burst.
            request_worker_collect();
            on_worker_park();
            let limited = snapshot();
            assert_eq!(
                limited.park_collects, after_first.park_collects,
                "generation request must not bypass the per-worker rate limit"
            );
            assert_eq!(
                limited.park_collects_rate_limited,
                after_first.park_collects_rate_limited + 1,
                "rate-limited request must be observable"
            );

            // Once the interval is clear, the pending generation is honored.
            reset_park_interval_for_test();
            on_worker_park();
            let recovered = snapshot();
            assert_eq!(
                recovered.park_collects,
                after_first.park_collects + 1,
                "the pending generation must be collected after the interval"
            );
            assert_eq!(
                recovered.park_collects_rate_limited, limited.park_collects_rate_limited,
                "the honored generation is not counted as rate limited"
            );
            on_worker_stop();
        });
        handle.join().expect("worker hook thread");
    }

    #[test]
    fn stop_collects_and_balances_live_worker_accounting() {
        let _guard = lock_tests();
        let before = snapshot();
        let handle = std::thread::spawn(move || {
            on_worker_start();
            assert!(
                snapshot().worker_threads_live > before.worker_threads_live,
                "start hook must account the live worker"
            );
            on_worker_stop();
        });
        handle.join().expect("worker hook thread");
        let after = snapshot();
        assert_eq!(
            after.stop_collects,
            before.stop_collects + 1,
            "stop hook must collect the dying worker's heap"
        );
        assert_eq!(
            after.worker_threads_stopped,
            before.worker_threads_stopped + 1
        );
        assert_eq!(
            after.worker_threads_live, before.worker_threads_live,
            "start/stop hooks must leave the live count balanced"
        );
    }

    #[test]
    fn installed_hooks_run_on_tokio_worker_threads() {
        let _guard = lock_tests();
        let before = snapshot();
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(1).enable_all();
        install_tokio_worker_hooks(&mut builder);
        let rt = builder.build().expect("test runtime");

        rt.spawn(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while snapshot().park_collects <= before.park_collects && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let live = snapshot();
        assert!(
            live.worker_threads_started > before.worker_threads_started,
            "start hook must run on the real runtime worker"
        );
        assert!(
            live.park_collects > before.park_collects,
            "park hook must collect on the real runtime worker, not a helper thread"
        );

        rt.shutdown_timeout(Duration::from_secs(2));
        let after = snapshot();
        assert!(
            after.stop_collects > before.stop_collects,
            "stop hook must run when the runtime worker exits"
        );
        assert_eq!(
            after.worker_threads_live, before.worker_threads_live,
            "runtime shutdown must balance the live worker count"
        );
    }

    #[test]
    fn coverage_stays_false_while_pingora_workers_are_unhooked() {
        let _guard = lock_tests();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            on_worker_start();
            started_tx.send(()).expect("signal start");
            release_rx.recv().expect("hold worker live");
            on_worker_stop();
        });
        started_rx.recv().expect("worker started");
        let saved = UNHOOKED_PINGORA_WORKERS.load(Ordering::Relaxed);

        set_unhooked_pingora_workers(4);
        let partial = snapshot();
        assert!(
            partial.worker_threads_live >= 1,
            "a hooked worker is live for this assertion"
        );
        assert!(
            !partial.covers_all_workers(),
            "live hooks must not be reported as full coverage while Pingora workers are unhooked"
        );

        set_unhooked_pingora_workers(0);
        assert!(
            snapshot().covers_all_workers(),
            "coverage is complete only when no unhooked worker population remains"
        );

        set_unhooked_pingora_workers(saved as usize);
        release_tx.send(()).expect("release worker");
        handle.join().expect("worker hook thread");
    }
}
