//! Issue 34 production allocator validation.
//!
//! This is an integration-test binary, so it can install its own global
//! allocator. It installs the **same** allocator as the production binary
//! (`mimalloc::MiMalloc`) and calls the production
//! `zelvik_edge::memory_reclaim::configure_allocator()`; the tests then prove
//! allocations really route through mimalloc (via `mi_process_info` commit
//! growth) and read the configured options back.
//!
//! There is no production allocator stats agent at Edge 40fda4f. This file
//! therefore contains the test-only `StatsAgent` that samples
//! `libmimalloc_sys` plus the strict `/proc` and cgroup v2 surfaces. Missing
//! Linux statistics panic; they are never reported as zero.
//!
//! The HTTP/TLS/h2/SNI soak lives in `scripts/memory/soak.py`. Its usual
//! target, `bench-defense`, is a **component lane** (real EdgeProxy /
//! HttpProxyManager listeners with local config injection), not the production
//! startup path; a separate full-Edge lane exists but is blocked until a
//! control-plane fixture is available. This file does not pretend to be either
//! soak.
//!
//! Run serially: `cargo test --test issue34_memory -- --test-threads=1`.
//! A process-wide mutex also serialises allocator observations.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::ffi::CStr;
use std::io::{BufRead, BufReader};
use std::os::raw::{c_char, c_int};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

// Match the production binary. Without this, the process would use the system
// allocator and every assertion below would be meaningless.
#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

// mimalloc v3 `mi_option_t` slots (c_src/mimalloc/v3/include/mimalloc.h).
// The production `configure_allocator` must enable page reclaim on free and
// must not disallow arena allocation.
const MI_OPTION_DISALLOW_ARENA_ALLOC: c_int = 26;
const MI_OPTION_PAGE_RECLAIM_ON_FREE: c_int = 35;

const MIB: u64 = 1024 * 1024;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn configure_production_allocator() {
    zelvik_edge::memory_reclaim::configure_allocator();
}

fn mi_option(option: c_int) -> i64 {
    unsafe { libmimalloc_sys::mi_option_get(option) as i64 }
}

fn collect_mimalloc(force: bool) {
    unsafe { libmimalloc_sys::mi_collect(force) };
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessInfo {
    commit_current: usize,
    peak_commit: usize,
    rss_current: usize,
    peak_rss: usize,
    page_faults: usize,
}

fn process_info() -> ProcessInfo {
    let mut commit: usize = 0;
    let mut peak_commit: usize = 0;
    let mut rss: usize = 0;
    let mut peak_rss: usize = 0;
    let mut faults: usize = 0;
    unsafe {
        libmimalloc_sys::mi_process_info(
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut rss,
            &mut peak_rss,
            &mut commit,
            &mut peak_commit,
            &mut faults,
        );
    }
    ProcessInfo {
        commit_current: commit,
        peak_commit,
        rss_current: rss,
        peak_rss,
        page_faults: faults,
    }
}

fn mimalloc_stats_json() -> String {
    let mut buffer = vec![0u8; 512 * 1024];
    let pointer = unsafe {
        libmimalloc_sys::mi_stats_get_json(buffer.len(), buffer.as_mut_ptr().cast::<c_char>())
    };
    assert!(
        !pointer.is_null(),
        "mi_stats_get_json returned null (buffer too small or stats unavailable)"
    );
    let text = unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned();
    assert!(!text.is_empty(), "mi_stats_get_json returned an empty document");
    text
}

// ---------------------------------------------------------------------------
// Strict Linux statistics
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct StatusStats {
    rss: u64,
    anon: u64,
    swap: u64,
    threads: u64,
}

#[derive(Clone, Copy, Debug)]
struct RollupStats {
    pss: u64,
    pss_anon: u64,
    anon_huge: u64,
}

fn required_text(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("required Linux statistic {path} unavailable: {error}"))
}

fn parse_kib(value: &str, key: &str, source: &str) -> Result<u64, String> {
    let mut parts = value.split_whitespace();
    let number = parts
        .next()
        .ok_or_else(|| format!("{source}: {key} has no value"))?;
    let parsed: u64 = number
        .parse()
        .map_err(|_| format!("{source}: {key} is not an integer: {number:?}"))?;
    if let Some(unit) = parts.next()
        && unit != "kB"
    {
        return Err(format!("{source}: {key} has unexpected unit {unit:?}"));
    }
    Ok(parsed * 1024)
}

fn field_u64<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, rest) = line.split_once(':')?;
        (name.trim() == key).then_some(rest)
    })
}

fn parse_status(text: &str, source: &str) -> Result<StatusStats, String> {
    let mut values = BTreeMap::new();
    for key in ["VmRSS", "RssAnon", "VmSwap"] {
        let raw = field_u64(text, key)
            .ok_or_else(|| format!("{source}: missing required field {key}"))?;
        values.insert(key, parse_kib(raw, key, source)?);
    }
    let threads = field_u64(text, "Threads")
        .ok_or_else(|| format!("{source}: missing required field Threads"))?
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{source}: Threads is not an integer"))?;
    Ok(StatusStats {
        rss: values["VmRSS"],
        anon: values["RssAnon"],
        swap: values["VmSwap"],
        threads,
    })
}

fn parse_rollup(text: &str, source: &str) -> Result<RollupStats, String> {
    let mut values = BTreeMap::new();
    for key in ["Rss", "Pss", "Pss_Anon", "Anonymous", "AnonHugePages", "Swap"] {
        let raw = field_u64(text, key)
            .ok_or_else(|| format!("{source}: missing required field {key}"))?;
        values.insert(key, parse_kib(raw, key, source)?);
    }
    Ok(RollupStats {
        pss: values["Pss"],
        pss_anon: values["Pss_Anon"],
        anon_huge: values["AnonHugePages"],
    })
}

fn parse_starttime(text: &str, source: &str) -> Result<u64, String> {
    let right = text
        .rfind(')')
        .ok_or_else(|| format!("{source}: malformed stat line"))?;
    let rest: Vec<&str> = text[right + 1..].split_whitespace().collect();
    // rest[0] is field 3 (state); field 22 (starttime) is index 19.
    let value = rest
        .get(19)
        .ok_or_else(|| format!("{source}: stat line has too few fields"))?;
    value
        .parse::<u64>()
        .map_err(|_| format!("{source}: starttime is not an integer: {value:?}"))
}

fn parse_cgroup_events(text: &str, source: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let key = parts
            .next()
            .ok_or_else(|| format!("{source}: empty line"))?;
        let value = parts
            .next()
            .ok_or_else(|| format!("{source}: missing value for {key}"))?;
        if parts.next().is_some() {
            return Err(format!("{source}: extra fields on line {line:?}"));
        }
        let parsed: u64 = value
            .parse()
            .map_err(|_| format!("{source}: {key} is not an integer"))?;
        values.insert(key.to_string(), parsed);
    }
    for key in ["low", "high", "max", "oom", "oom_kill"] {
        if !values.contains_key(key) {
            return Err(format!("{source}: missing required counter {key}"));
        }
    }
    Ok(values)
}

fn parse_cgroup_limit(text: &str, source: &str) -> Result<Option<u64>, String> {
    let trimmed = text.trim();
    if trimmed == "max" {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{source}: not an integer or 'max': {trimmed:?}"))
}

fn cgroup_dir(pid: u32) -> String {
    let text = required_text(&format!("/proc/{pid}/cgroup"));
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        if parts.next() == Some("0") && parts.next() == Some("") {
            let relative = parts.next().unwrap_or("");
            if relative.starts_with('/') {
                let root = std::env::var("ISSUE34_CGROUP_ROOT")
                    .unwrap_or_else(|_| "/sys/fs/cgroup".to_string());
                return format!("{}{}", root.trim_end_matches('/'), relative);
            }
        }
    }
    panic!("no cgroup v2 (0::) entry for pid {pid}; cgroup v1 is unsupported")
}

fn fd_count(pid: u32) -> usize {
    let path = format!("/proc/{pid}/fd");
    let directory = std::fs::read_dir(&path)
        .unwrap_or_else(|error| panic!("cannot list {path}: {error}"));
    let mut count = 0;
    for entry in directory {
        entry.unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
        count += 1;
    }
    count
}

fn maps_summary(pid: u32) -> (u64, u64) {
    let path = format!("/proc/{pid}/maps");
    let file = std::fs::File::open(&path)
        .unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
    let mut count = 0u64;
    let mut bytes = 0u64;
    for line in BufReader::new(file).lines() {
        let line = line.unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
        count += 1;
        if count > 5_000_000 {
            panic!("{path}: mapping count exceeded the 5,000,000 guard");
        }
        let range = line
            .split_whitespace()
            .next()
            .unwrap_or_else(|| panic!("{path}: malformed mapping {line:?}"));
        let (start, end) = range
            .split_once('-')
            .unwrap_or_else(|| panic!("{path}: malformed range {range:?}"));
        let start = u64::from_str_radix(start, 16)
            .unwrap_or_else(|_| panic!("{path}: non-hex range {range:?}"));
        let end = u64::from_str_radix(end, 16)
            .unwrap_or_else(|_| panic!("{path}: non-hex range {range:?}"));
        bytes += end.saturating_sub(start);
    }
    (count, bytes)
}

// ---------------------------------------------------------------------------
// Test-only stats agent
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Sample {
    t_seconds: f64,
    commit_current: usize,
    peak_commit: usize,
    peak_rss: usize,
    rss: u64,
    anon: u64,
    pss: u64,
    anon_huge: u64,
    swap: u64,
    threads: u64,
    fds: usize,
    maps_count: u64,
    maps_bytes: u64,
    cgroup_current: u64,
    cgroup_events: BTreeMap<String, u64>,
}

struct StatsAgent {
    pid: u32,
    starttime: u64,
    started: Instant,
}

impl StatsAgent {
    fn new(pid: u32) -> Self {
        let source = format!("/proc/{pid}/stat");
        let starttime =
            parse_starttime(&required_text(&source), &source).expect("parse starttime");
        Self {
            pid,
            starttime,
            started: Instant::now(),
        }
    }

    fn verify_identity(&self) -> u64 {
        let source = format!("/proc/{}/stat", self.pid);
        let current = parse_starttime(&required_text(&source), &source)
            .unwrap_or_else(|error| panic!("process {} disappeared: {error}", self.pid));
        assert_eq!(
            current, self.starttime,
            "pid {} was reused (starttime {} -> {})",
            self.pid, self.starttime, current
        );
        current
    }

    fn capture(&self) -> Sample {
        self.verify_identity();
        let status_path = format!("/proc/{}/status", self.pid);
        let status = parse_status(&required_text(&status_path), &status_path)
            .unwrap_or_else(|error| panic!("{error}"));
        let rollup_path = format!("/proc/{}/smaps_rollup", self.pid);
        let rollup = parse_rollup(&required_text(&rollup_path), &rollup_path)
            .unwrap_or_else(|error| panic!("{error}"));
        let directory = cgroup_dir(self.pid);
        let cgroup_current: u64 = required_text(&format!("{directory}/memory.current"))
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{directory}/memory.current is not an integer"));
        let events_path = format!("{directory}/memory.events");
        let cgroup_events = parse_cgroup_events(&required_text(&events_path), &events_path)
            .unwrap_or_else(|error| panic!("{error}"));
        let (maps_count, maps_bytes) = maps_summary(self.pid);
        let info = process_info();
        Sample {
            t_seconds: self.started.elapsed().as_secs_f64(),
            commit_current: info.commit_current,
            peak_commit: info.peak_commit,
            peak_rss: info.peak_rss,
            rss: status.rss,
            anon: status.anon,
            pss: rollup.pss,
            anon_huge: rollup.anon_huge,
            swap: status.swap,
            threads: status.threads,
            fds: fd_count(self.pid),
            maps_count,
            maps_bytes,
            cgroup_current,
            cgroup_events,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn global_allocator_is_mimalloc_not_the_system_allocator() {
    let _guard = lock();
    let name = std::any::type_name_of_val(&GLOBAL_ALLOCATOR);
    assert!(
        name.contains("MiMalloc"),
        "global allocator is {name}, expected mimalloc::MiMalloc"
    );
    assert!(
        !name.contains("System"),
        "global allocator is the system allocator ({name}); Issue 34 requires mimalloc"
    );

    configure_production_allocator();
    collect_mimalloc(true);
    let before = process_info();

    let mut blocks: Vec<Vec<u8>> = (0..128).map(|_| vec![0xA5u8; 1024 * 1024]).collect();
    for block in &mut blocks {
        for offset in (0..block.len()).step_by(4096) {
            block[offset] = block[offset].wrapping_add(1);
        }
    }
    let during = process_info();
    assert!(
        during.commit_current >= before.commit_current + 64 * 1024 * 1024,
        "mimalloc commit did not grow with a 128 MiB allocation: before={} during={}",
        before.commit_current,
        during.commit_current
    );
    assert!(
        during.peak_commit >= during.commit_current,
        "peak commit {} < current commit {}",
        during.peak_commit,
        during.commit_current
    );

    drop(blocks);
    collect_mimalloc(true);
    let after = process_info();
    assert!(
        after.commit_current <= during.commit_current,
        "commit grew after freeing and collecting: during={} after={}",
        during.commit_current,
        after.commit_current
    );
}

#[test]
fn production_configure_allocator_readback_matches_v3_contract() {
    let _guard = lock();
    let version = unsafe { libmimalloc_sys::mi_version() };
    assert!(
        version >= 30_000,
        "expected mimalloc v3 (>=30000), linked version is {version}"
    );

    configure_production_allocator();
    assert_eq!(
        mi_option(MI_OPTION_PAGE_RECLAIM_ON_FREE),
        1,
        "configure_allocator must enable mi_option_page_reclaim_on_free"
    );
    assert_eq!(
        mi_option(MI_OPTION_DISALLOW_ARENA_ALLOC),
        0,
        "configure_allocator must not disallow arena allocation"
    );

    // Idempotent: a second call must not drift the readback values.
    configure_production_allocator();
    assert_eq!(mi_option(MI_OPTION_PAGE_RECLAIM_ON_FREE), 1);
    assert_eq!(mi_option(MI_OPTION_DISALLOW_ARENA_ALLOC), 0);
}

#[test]
fn deterministic_1024_windows_with_cross_worker_handoff() {
    let _guard = lock();
    configure_production_allocator();
    collect_mimalloc(true);

    const WINDOWS: usize = 1024;
    const WORKERS: usize = 4;
    const BLOCKS: usize = 16;
    const BLOCK_BYTES: usize = 4096;

    let barriers = Arc::new((
        Barrier::new(WORKERS + 1),
        Barrier::new(WORKERS + 1),
        Barrier::new(WORKERS + 1),
        Barrier::new(WORKERS + 1),
    ));
    let allocated = Arc::new(AtomicU64::new(0));
    let mut senders = Vec::with_capacity(WORKERS);
    let mut receivers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(BLOCKS);
        senders.push(sender);
        receivers.push(receiver);
    }

    let mut handles = Vec::with_capacity(WORKERS);
    let mut receiver_iter = receivers.into_iter();
    for worker in 0..WORKERS {
        let receiver = receiver_iter.next().expect("receiver per worker");
        let next_sender = senders[(worker + 1) % WORKERS].clone();
        let barriers = Arc::clone(&barriers);
        let allocated = Arc::clone(&allocated);
        handles.push(thread::spawn(move || {
            let seed = worker as u8;
            for window in 0..WINDOWS {
                barriers.0.wait();
                let mut blocks = Vec::with_capacity(BLOCKS);
                for index in 0..BLOCKS {
                    let mut block = vec![seed.wrapping_add(index as u8); BLOCK_BYTES];
                    block[window % BLOCK_BYTES] = 1;
                    blocks.push(block);
                }
                allocated.fetch_add(BLOCKS as u64, Ordering::Relaxed);
                for block in blocks {
                    next_sender.send(block).expect("cross-worker handoff send");
                }
                barriers.1.wait();
                barriers.2.wait();
                for _ in 0..BLOCKS {
                    let block = receiver.recv().expect("cross-worker handoff receive");
                    drop(block);
                }
                barriers.3.wait();
            }
        }));
    }

    let pid = std::process::id();
    let agent = StatsAgent::new(pid);
    let mut samples = Vec::with_capacity(WINDOWS * 2 + 1);
    samples.push(agent.capture());
    for _ in 0..WINDOWS {
        barriers.0.wait();
        barriers.1.wait();
        // Main samples while all handed-off blocks are still live.
        samples.push(agent.capture());
        barriers.2.wait();
        barriers.3.wait();
        // Main samples after the receiving worker dropped every block.
        samples.push(agent.capture());
    }
    for handle in handles {
        handle.join().expect("worker thread");
    }

    assert_eq!(
        allocated.load(Ordering::Relaxed),
        (WINDOWS * WORKERS * BLOCKS) as u64,
        "every window must allocate its deterministic block count"
    );
    assert_eq!(samples.len(), WINDOWS * 2 + 1);
    for sample in &samples {
        assert!(sample.rss > 0, "VmRSS must never be reported as zero");
        assert!(sample.pss > 0, "Pss must never be reported as zero");
        assert!(sample.commit_current > 0, "mimalloc commit must be nonzero");
        assert!(sample.cgroup_current > 0, "cgroup memory.current must be nonzero");
        assert!(sample.threads > 0);
        assert!(sample.fds > 0);
        assert!(sample.maps_count > 0);
        assert!(sample.maps_bytes > 0);
        assert!(sample.cgroup_events.contains_key("oom_kill"));
    }

    let first = &samples[0];
    let peak = samples
        .iter()
        .map(|sample| sample.commit_current)
        .max()
        .expect("samples");
    assert!(
        peak > first.commit_current + 1024 * 1024,
        "1024 windows never produced a mimalloc commit increase: first={} peak={peak}",
        first.commit_current
    );

    collect_mimalloc(true);
    let final_sample = agent.capture();
    assert!(
        final_sample.commit_current <= first.commit_current + 32 * 1024 * 1024,
        "allocator commit grew unboundedly across 1024 windows: first={} final={}",
        first.commit_current,
        final_sample.commit_current
    );
    assert!(
        final_sample.rss <= first.rss + 64 * 1024 * 1024,
        "RSS grew unboundedly across 1024 windows: first={} final={}",
        first.rss,
        final_sample.rss
    );
}

#[test]
fn sustained_cross_worker_handoff_frees_on_receiver() {
    let _guard = lock();
    configure_production_allocator();
    collect_mimalloc(true);
    let agent = StatsAgent::new(std::process::id());
    let baseline = agent.capture();

    const ROUNDS: usize = 2048;
    const BLOCK_BYTES: usize = 64 * 1024;
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(32);
    let producer = thread::spawn(move || {
        for round in 0..ROUNDS {
            let mut block = vec![round as u8; BLOCK_BYTES];
            block[round % BLOCK_BYTES] = 1;
            sender.send(block).expect("producer send");
        }
    });
    let consumer = thread::spawn(move || {
        let mut received = 0usize;
        while let Ok(block) = receiver.recv() {
            drop(block);
            received += 1;
        }
        received
    });
    producer.join().expect("producer");
    // The producer owns `sender`; when it returns the sender drops and the
    // bounded channel disconnects after the consumer drains it.
    let received = consumer.join().expect("consumer");
    assert_eq!(received, ROUNDS);

    collect_mimalloc(true);
    let after = agent.capture();
    assert!(
        after.commit_current <= baseline.commit_current + 32 * 1024 * 1024,
        "cross-worker frees leaked allocator commit: baseline={} after={}",
        baseline.commit_current,
        after.commit_current
    );
    assert_eq!(after.swap, 0, "the test process must not use swap");
}

#[test]
fn stopped_workers_release_abandoned_pages_while_idle_workers_retain() {
    let _guard = lock();
    configure_production_allocator();
    collect_mimalloc(true);
    let agent = StatsAgent::new(std::process::id());
    let baseline = agent.capture();

    const GROUPS: usize = 4;
    const BLOCKS: usize = 32;
    const BLOCK_BYTES: usize = 1024 * 1024;
    let barrier = Arc::new(Barrier::new(GROUPS * 2 + 1));
    let mut stopped_handles = Vec::new();
    let mut idle_handles = Vec::new();
    let mut idle_releases = Vec::new();

    for group in 0..(GROUPS * 2) {
        let barrier = Arc::clone(&barrier);
        if group < GROUPS {
            stopped_handles.push(thread::spawn(move || {
                let blocks: Vec<Vec<u8>> = (0..BLOCKS)
                    .map(|index| vec![(group + index) as u8; BLOCK_BYTES])
                    .collect();
                for block in &blocks {
                    // Touch one byte per page so the pages are resident.
                    for offset in (0..block.len()).step_by(4096) {
                        std::hint::black_box(block[offset]);
                    }
                }
                barrier.wait();
                drop(blocks);
                // Thread exits here; mimalloc retires the heap as abandoned.
            }));
        } else {
            let (release_sender, release_receiver) = mpsc::sync_channel::<()>(1);
            idle_releases.push(release_sender);
            idle_handles.push(thread::spawn(move || {
                let blocks: Vec<Vec<u8>> = (0..BLOCKS)
                    .map(|index| vec![(group + index) as u8; BLOCK_BYTES])
                    .collect();
                for block in &blocks {
                    for offset in (0..block.len()).step_by(4096) {
                        std::hint::black_box(block[offset]);
                    }
                }
                barrier.wait();
                // Stay alive and keep the allocations resident: this is the
                // "idle but not stopped" control group. Dropping here would
                // make the comparison against stopped workers meaningless.
                let _ = release_receiver.recv();
                drop(blocks);
            }));
        }
    }

    barrier.wait();
    let all_live = agent.capture();
    assert!(
        all_live.commit_current > baseline.commit_current + 64 * 1024 * 1024,
        "expected the live buffers to be visible to mimalloc: baseline={} live={}",
        baseline.commit_current,
        all_live.commit_current
    );

    for handle in stopped_handles {
        handle.join().expect("stopped worker");
    }

    // Bounded retry: abandoned segments are purged by the allocator.
    let mut after_stop = agent.capture();
    let deadline = Instant::now() + Duration::from_secs(10);
    while after_stop.commit_current > all_live.commit_current.saturating_sub(8 * 1024 * 1024)
        && Instant::now() < deadline
    {
        collect_mimalloc(true);
        thread::sleep(Duration::from_millis(50));
        after_stop = agent.capture();
    }
    assert!(
        after_stop.commit_current + 8 * 1024 * 1024 <= all_live.commit_current,
        "stopped workers did not release abandoned pages: live={} after_stop={}",
        all_live.commit_current,
        after_stop.commit_current
    );

    for release in idle_releases {
        let _ = release.send(());
    }
    for handle in idle_handles {
        handle.join().expect("idle worker");
    }
    collect_mimalloc(true);
    let after_idle = agent.capture();
    assert!(
        after_idle.commit_current <= all_live.commit_current,
        "idle workers retained more than the live peak: live={} after_idle={}",
        all_live.commit_current,
        after_idle.commit_current
    );
    assert_eq!(after_idle.swap, 0);
}

#[test]
fn missing_linux_stats_fail_and_are_never_zero() {
    let _guard = lock();
    assert!(parse_status("", "/proc/self/status").is_err());
    assert!(
        parse_status(
            "VmRSS: 1 kB\nRssAnon: 1 kB\nThreads: 1\n",
            "/proc/self/status"
        )
        .is_err(),
        "missing VmSwap must fail"
    );
    assert!(
        parse_status(
            "VmRSS: 1 kB\nRssAnon: 1 kB\nVmSwap: 0 kB\nThreads: 1\n",
            "/proc/self/status"
        )
        .is_ok(),
        "a present zero swap counter is valid"
    );
    assert!(
        parse_rollup(
            "Rss: 1 kB\nPss: 1 kB\nPss_Anon: 1 kB\nAnonymous: 1 kB\nSwap: 0 kB\n",
            "/proc/self/smaps_rollup"
        )
        .is_err(),
        "missing AnonHugePages must fail"
    );
    assert!(
        parse_cgroup_events("low 0\nhigh 0\nmax 0\noom 0\n", "memory.events").is_err(),
        "missing oom_kill must fail"
    );
    assert_eq!(
        parse_cgroup_events("low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n", "memory.events")
            .expect("complete events")
            .get("oom_kill"),
        Some(&0),
        "a present zero oom_kill is preserved"
    );
    assert!(parse_starttime("1 (comm) S 1\n", "/proc/self/stat").is_err());
    assert!(parse_cgroup_limit("max", "memory.max").is_ok_and(|value| value.is_none()));
    assert_eq!(
        parse_cgroup_limit("0", "memory.swap.max").expect("zero limit"),
        Some(0),
        "a present zero limit is preserved"
    );
    assert!(parse_cgroup_limit("bogus", "memory.max").is_err());

    // PID reuse detection: a different starttime must be rejected.
    let source = "/proc/self/stat";
    let starttime = parse_starttime(&required_text(source), source).expect("starttime");
    let mismatch = parse_starttime(&required_text(source), source).expect("starttime");
    assert_eq!(starttime, mismatch);
    assert_ne!(starttime.wrapping_add(1), mismatch);
}

#[test]
fn mimalloc_json_exposes_process_and_allocator_cache_metrics() {
    let _guard = lock();
    configure_production_allocator();
    let mut block = vec![0x5Au8; 8 * 1024 * 1024];
    for offset in (0..block.len()).step_by(4096) {
        block[offset] = block[offset].wrapping_add(1);
    }
    let json = mimalloc_stats_json();
    let value: serde_json::Value = serde_json::from_str(&json).expect("mimalloc JSON parses");
    let process = value
        .get("process")
        .and_then(serde_json::Value::as_object)
        .expect("process object");
    for key in ["commit_current", "peak_commit", "rss_current", "page_faults"] {
        let field = process
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_else(|| panic!("mimalloc JSON missing numeric process.{key}"));
        if key != "page_faults" {
            assert!(field > 0, "process.{key} must be nonzero after allocation");
        }
    }
    assert!(
        value
            .get("malloc_bins")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|bins| !bins.is_empty()),
        "mimalloc JSON must expose malloc_bins (allocator cache counters)"
    );
    drop(block);
    collect_mimalloc(true);
}
