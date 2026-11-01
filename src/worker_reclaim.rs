//! Deterministic mimalloc heap collection on self-owned Tokio worker threads.
//!
//! `mi_collect` only purges the calling thread's mimalloc heap. The cache
//! janitor used to rely on its Tokio task migrating between runtime workers
//! ("spawn lottery") so that, over time, different heaps were visited. That is
//! neither deterministic nor complete: a task may run on the same worker
//! forever, and a busy worker may never be visited at all. Tokio's
//! `on_thread_start` / `on_thread_park` / `on_thread_stop` callbacks run on the
//! actual worker threads. Owned tasks also check after polling; park/poll checks
//! share a bounded schedule. Short-lived RPC tasks relinquish on completion.
//!
//! # Coverage boundary
//!
//! These hooks are installed only on the runtime this process builds itself in
//! `main` (`tokio::runtime::Builder::new_multi_thread`), including the custom
//! HTTP/TCP/UDP accept loops. Pingora builds its separate control/background
//! runtimes internally; those threads are not covered. Tokio blocking-pool
//! threads run start/stop hooks but never the park hook; durable storage work
//! therefore uses [`spawn_storage_work`] to relinquish its heap on completion.
//! Application-owned transport tasks check the same generation and idle deadline
//! after each poll: a runnable worker need not enter Tokio's park callback.
//! A worker that stays parked has no periodic callback until it wakes and parks again.
//! [`WorkerReclaimSnapshot`] therefore reports the hooked worker population
//! and the configured-but-unhooked Pingora population separately;
//! [`WorkerReclaimSnapshot::covers_all_workers`] stays false until an actual
//! hook exists for every worker. Never describe partial coverage as full
//! coverage.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Hard floor between timed park/poll collects on the same worker. A pressure
/// request remains pending until a later boundary after this interval.
/// RPC/storage completion handoffs follow task lifetime instead of this timer.
const PARK_COLLECT_MIN_INTERVAL: Duration = Duration::from_millis(250);

/// A worker that parks or polls an owned task after this interval collects
/// without a new generation. A continuously parked worker cannot run the check.
const PARK_COLLECT_IDLE_INTERVAL: Duration = Duration::from_secs(30);

static COLLECT_GENERATION: AtomicU64 = AtomicU64::new(1);
static COLLECT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_STARTED: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_LIVE: AtomicU64 = AtomicU64::new(0);
static WORKER_THREADS_STOPPED: AtomicU64 = AtomicU64::new(0);
static PARK_COLLECTS: AtomicU64 = AtomicU64::new(0);
static POLL_COLLECTS: AtomicU64 = AtomicU64::new(0);
static RPC_COMPLETION_COLLECTS: AtomicU64 = AtomicU64::new(0);
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
    pub poll_collects: u64,
    pub rpc_completion_collects: u64,
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
        poll_collects: POLL_COLLECTS.load(Ordering::Relaxed),
        rpc_completion_collects: RPC_COMPLETION_COLLECTS.load(Ordering::Relaxed),
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

/// Run durable storage work on Tokio's blocking pool and relinquish its heap
/// before returning to the pool. Blocking threads never run `on_thread_park`;
/// retaining ownership would delay frees arriving after task completion.
pub(crate) fn spawn_storage_work<F, T>(work: F) -> tokio::task::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        struct CollectOnCompletion;
        impl Drop for CollectOnCompletion {
            fn drop(&mut self) {
                crate::memory_reclaim::relinquish_thread_heap();
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

/// Check generation and idle collection after polling an owned transport task.
/// Busy Tokio workers can keep polling runnable tasks without entering `park`.
/// They still need the idle deadline after acknowledging a generation, since
/// later allocations and cross-thread frees can populate a new heap.
/// The pinned future stays in place and the existing per-worker rate limit applies.
pub(crate) async fn with_worker_reclaim<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(move |cx| {
        let result = future.as_mut().poll(cx);
        if matches!(collect_decision(), ParkDecision::Collect) {
            crate::memory_reclaim::relinquish_thread_heap();
            POLL_COLLECTS.fetch_add(1, Ordering::Relaxed);
        }
        result
    })
    .await
}

/// Relinquish short-lived RPC task pages after its future has been dropped.
/// A completed upload/driver may be the last task before a rate-limited park;
/// later frees must not depend on another poll or a worker wakeup.
/// Cancellation and unwinding drop the inner future before this guard.
pub(crate) async fn with_rpc_worker_reclaim<F: std::future::Future>(future: F) -> F::Output {
    struct RpcCompletion;
    impl Drop for RpcCompletion {
        fn drop(&mut self) {
            let generation = COLLECT_GENERATION.load(Ordering::Acquire);
            crate::memory_reclaim::relinquish_thread_heap();
            WORKER_COLLECT_STATE.with(|cell| {
                cell.set(WorkerCollectState {
                    generation,
                    last_collect: Some(Instant::now()),
                });
            });
            RPC_COMPLETION_COLLECTS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _completion = RpcCompletion;
    with_worker_reclaim(future).await
}

/// Executor for RPC driver and balance tasks spawned through Tonic's public API.
#[derive(Clone, Copy)]
pub(crate) struct ReclaimExecutor;

impl<F> hyper::rt::Executor<F> for ReclaimExecutor
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        tokio::spawn(with_rpc_worker_reclaim(future));
    }
}

fn collect_decision() -> ParkDecision {
    let generation = COLLECT_GENERATION.load(Ordering::Acquire);
    WORKER_COLLECT_STATE.with(|cell| {
        let state = cell.get();
        let now = Instant::now();
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
    })
}

fn on_worker_park() {
    match collect_decision() {
        ParkDecision::Collect => {
            // A collect cannot see frees that arrive after this worker parks.
            // Relinquish ownership so those frees need not wake this worker.
            crate::memory_reclaim::relinquish_thread_heap();
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
    fn requested_handoff_runs_while_transport_task_stays_runnable() {
        assert_busy_task_handoff(false, false);
    }

    #[test]
    fn rpc_executor_honors_requested_handoff_on_a_runnable_task() {
        assert_busy_task_handoff(true, false);
    }

    #[test]
    fn busy_worker_honors_idle_deadline_without_a_new_generation() {
        assert_busy_task_handoff(true, true);
    }

    fn assert_busy_task_handoff(use_executor: bool, idle_due: bool) {
        let _guard = lock_tests();
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(1).enable_all();
        install_tokio_worker_hooks(&mut builder);
        let runtime = builder.build().expect("busy worker runtime");
        let (blocks_tx, blocks_rx) = mpsc::channel();
        let freed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_freed = freed.clone();
        let work = async move {
            let before = snapshot();
            WORKER_COLLECT_STATE.with(|cell| {
                cell.set(WorkerCollectState {
                    generation: before.collect_generation,
                    last_collect: Some(if idle_due {
                        Instant::now()
                    } else {
                        Instant::now() - PARK_COLLECT_MIN_INTERVAL
                    }),
                });
            });
            let blocks = (0..8)
                .map(|index| {
                    let size = (96 + index * 32) * 1024;
                    let block = unsafe { libmimalloc_sys::mi_malloc_aligned(size, 8) };
                    assert!(!block.is_null());
                    unsafe { std::ptr::write_bytes(block.cast::<u8>(), 7, size) };
                    block as usize
                })
                .collect::<Vec<_>>();
            blocks_tx.send(blocks).expect("send live blocks");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut interval_aged = false;
            std::future::poll_fn(move |cx| {
                let freed = worker_freed.load(Ordering::Acquire);
                if idle_due && freed && !interval_aged {
                    // Move only the timestamp so the real 30-second deadline
                    // is deterministic without a 30-second test sleep.
                    WORKER_COLLECT_STATE.with(|cell| {
                        let mut state = cell.get();
                        state.last_collect = Some(Instant::now() - PARK_COLLECT_IDLE_INTERVAL);
                        cell.set(state);
                    });
                    interval_aged = true;
                }
                let after = snapshot();
                if freed && after.poll_collects > before.poll_collects {
                    assert_eq!(after.park_collects, before.park_collects);
                    if idle_due {
                        assert_eq!(after.collect_generation, before.collect_generation);
                        assert_eq!(after.collect_requests, before.collect_requests);
                    }
                    assert_eq!(large_thread_owned_bytes(), 0);
                    return std::task::Poll::Ready(());
                }
                assert!(
                    Instant::now() < deadline,
                    "busy worker ignored reclaim request"
                );
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        };
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let _runtime_guard = runtime.enter();
        if use_executor {
            hyper::rt::Executor::execute(&ReclaimExecutor, async move {
                work.await;
                done_tx.send(()).expect("signal RPC driver completion");
            });
        } else {
            runtime.spawn(with_worker_reclaim(async move {
                work.await;
                done_tx.send(()).expect("signal transport completion");
            }));
        }
        for block in blocks_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker blocks")
        {
            unsafe { libmimalloc_sys::mi_free(block as *mut std::ffi::c_void) };
        }
        if !idle_due {
            request_worker_collect();
        }
        freed.store(true, Ordering::Release);
        runtime.block_on(done_rx).expect("busy worker task");
    }

    #[test]
    fn rpc_completion_releases_late_frees_on_success_error_panic_and_cancel() {
        let _guard = lock_tests();
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(1).enable_all();
        install_tokio_worker_hooks(&mut builder);
        let runtime = builder.build().expect("RPC completion runtime");
        for outcome in 0..4 {
            let (blocks_tx, blocks_rx) = mpsc::channel();
            let before = snapshot();
            let task = runtime.spawn(with_rpc_worker_reclaim(async move {
                // The RPC finishes inside the existing park/poll floor.
                WORKER_COLLECT_STATE.with(|cell| {
                    cell.set(WorkerCollectState {
                        generation: COLLECT_GENERATION.load(Ordering::Acquire),
                        last_collect: Some(Instant::now()),
                    });
                });
                let blocks = (0..8)
                    .map(|index| {
                        let size = (96 + index * 32) * 1024;
                        let block = unsafe { libmimalloc_sys::mi_malloc_aligned(size, 8) };
                        assert!(!block.is_null());
                        unsafe { std::ptr::write_bytes(block.cast::<u8>(), 7, size) };
                        (block as usize, size)
                    })
                    .collect::<Vec<_>>();
                blocks_tx.send(blocks).expect("RPC live allocations");
                match outcome {
                    0 => Ok(7),
                    1 => Err("RPC failed"),
                    2 => panic!("injected RPC panic"),
                    _ => std::future::pending().await,
                }
            }));
            let blocks = blocks_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("RPC allocated blocks");
            if outcome == 3 {
                task.abort();
            }
            match outcome {
                0 => assert_eq!(runtime.block_on(task).unwrap(), Ok(7)),
                1 => assert_eq!(runtime.block_on(task).unwrap(), Err("RPC failed")),
                2 => assert!(runtime.block_on(task).unwrap_err().is_panic()),
                _ => assert!(runtime.block_on(task).unwrap_err().is_cancelled()),
            }
            assert_eq!(
                snapshot().rpc_completion_collects,
                before.rpc_completion_collects + 1
            );
            // Free only after the task and its completion guard have ended.
            for (block, size) in blocks {
                let bytes = unsafe { std::slice::from_raw_parts(block as *const u8, size) };
                assert!(bytes.iter().all(|&byte| byte == 7));
                unsafe { libmimalloc_sys::mi_free(block as *mut std::ffi::c_void) };
            }
            let retained = runtime
                .block_on(runtime.spawn(async { large_thread_owned_bytes() }))
                .expect("inspect RPC worker");
            assert_eq!(retained, 0, "RPC completion kept late frees thread-owned");
        }
    }

    #[test]
    fn park_handoff_preserves_live_data_and_releases_later_cross_thread_frees() {
        let _guard = lock_tests();
        let (allocated_tx, allocated_rx) = mpsc::channel();
        let (freed_tx, freed_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            on_worker_start();
            let blocks = (0..8)
                .map(|index| {
                    let size = (96 + index * 32) * 1024;
                    let block = unsafe { libmimalloc_sys::mi_malloc_aligned(size, 8) };
                    assert!(!block.is_null());
                    unsafe { std::ptr::write_bytes(block.cast::<u8>(), 7, size) };
                    (block as usize, size)
                })
                .collect::<Vec<_>>();
            on_worker_park();
            assert_eq!(
                large_thread_owned_bytes(),
                0,
                "parked worker kept page ownership"
            );
            allocated_tx.send(blocks).expect("send live allocations");
            freed_rx.recv().expect("late cross-thread frees");
            assert_eq!(large_thread_owned_bytes(), 0);
            for _ in 0..8 {
                let size = 128 * 1024;
                let block = unsafe { libmimalloc_sys::mi_malloc_aligned(size, 8) };
                assert!(!block.is_null());
                unsafe { std::ptr::write_bytes(block.cast::<u8>(), 9, size) };
                reset_park_interval_for_test();
                request_worker_collect();
                on_worker_park();
                let bytes = unsafe { std::slice::from_raw_parts(block.cast::<u8>(), size) };
                assert!(bytes.iter().all(|&byte| byte == 9));
                unsafe { libmimalloc_sys::mi_free(block) };
            }
            on_worker_stop();
        });
        for (block, size) in allocated_rx.recv().expect("worker live allocations") {
            let bytes = unsafe { std::slice::from_raw_parts(block as *const u8, size) };
            assert!(bytes.iter().all(|&byte| byte == 7));
            unsafe { libmimalloc_sys::mi_free(block as *mut std::ffi::c_void) };
        }
        freed_tx.send(()).expect("free complete");
        worker.join().expect("allocator handoff worker");
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
