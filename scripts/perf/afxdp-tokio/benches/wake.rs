//! Real XSK wake component benchmark. Client and relay share a current-thread
//! Tokio runtime; the legacy poll worker is the only extra application thread.

mod legacy {
    include!(concat!(env!("OUT_DIR"), "/legacy_worker.rs"));
}

use anyhow::{Context, Result, bail, ensure};
use std::ffi::{CStr, CString, c_void};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::io::{Interest, unix::AsyncFd};

use edge_afxdp_wake_bench::ffi::{
    NativeStats, edge_xsk_close, edge_xsk_echo, edge_xsk_failure_stage, edge_xsk_fd, edge_xsk_open,
    edge_xsk_peer_fd, edge_xsk_peer_receive, edge_xsk_peer_send, edge_xsk_reclaim, edge_xsk_stats,
};

use edge_afxdp_wake_bench::ffi::{edge_xsk_receive_only, edge_xsk_rx_pending};

fn checked(code: i32) -> Result<i32> {
    if code < 0 {
        // SAFETY: the fixture exposes an always-present, static C string.
        let stage = unsafe { CStr::from_ptr(edge_xsk_failure_stage()) }.to_string_lossy();
        bail!(
            "{stage}: {} (errno {})",
            io::Error::from_raw_os_error(-code),
            -code
        );
    }
    Ok(code)
}

struct Fixture {
    pointer: Option<NonNull<c_void>>,
}

impl Fixture {
    fn open(rx: &str, peer: &str) -> Result<Rc<Self>> {
        let rx = CString::new(rx)?;
        let peer = CString::new(peer)?;
        let mut code = 0;
        // SAFETY: both names and the error output live through this call.
        let pointer = unsafe { edge_xsk_open(rx.as_ptr(), peer.as_ptr(), &mut code) };
        checked(code)?;
        let pointer = NonNull::new(pointer).context("native fixture returned a null context")?;
        Ok(Rc::new(Self {
            pointer: Some(pointer),
        }))
    }

    fn raw(&self) -> *mut c_void {
        self.pointer.expect("fixture already closed").as_ptr()
    }

    fn socket_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: this live context owns the descriptor; the borrow cannot
        // outlive self. The C fixture is only used on this runtime thread.
        unsafe { BorrowedFd::borrow_raw(edge_xsk_fd(self.raw())) }
    }

    fn peer_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: same ownership as socket_fd, for the peer packet socket.
        unsafe { BorrowedFd::borrow_raw(edge_xsk_peer_fd(self.raw())) }
    }

    fn send(&self, sequence: u64) -> Result<()> {
        // SAFETY: the context remains live; this call only sends a peer frame.
        checked(unsafe { edge_xsk_peer_send(self.raw(), sequence) })?;
        Ok(())
    }

    fn receive(&self, sequence: u64) -> Result<bool> {
        // SAFETY: context lifetime is held by the caller. This consumes only
        // the peer socket, never the relay's SPSC rings.
        Ok(checked(unsafe { edge_xsk_peer_receive(self.raw(), sequence) })? == 1)
    }

    fn echo(&self, sequence: u64) -> Result<i32> {
        // SAFETY: one relay future on the current thread owns all ring access.
        checked(unsafe { edge_xsk_echo(self.raw(), sequence) })
    }

    fn pending(&self) -> Result<i32> {
        // SAFETY: this only peeks/cancels the one runtime thread's RX cursor.
        checked(unsafe { edge_xsk_rx_pending(self.raw()) })
    }

    fn receive_only(&self, sequence: u64) -> Result<bool> {
        // SAFETY: same SPSC ownership as echo; no TX is submitted here.
        Ok(checked(unsafe { edge_xsk_receive_only(self.raw(), sequence) })? == 1)
    }

    fn reclaim(&self) -> Result<()> {
        // SAFETY: same SPSC ownership as echo, after the relay has stopped.
        checked(unsafe { edge_xsk_reclaim(self.raw()) })?;
        Ok(())
    }

    fn stats(&self) -> NativeStats {
        let mut output = NativeStats::default();
        // SAFETY: initialized, correctly sized output; context stays live.
        unsafe { edge_xsk_stats(self.raw(), &mut output) };
        output
    }

    fn close(mut self) -> Result<()> {
        let pointer = self.pointer.take().context("fixture already closed")?;
        // SAFETY: all registered descriptors and the worker were closed
        // before Rc::try_unwrap. This consumes the sole native owner.
        checked(unsafe { edge_xsk_close(pointer.as_ptr()) })?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(pointer) = self.pointer.take() {
            // SAFETY: Rc owners pin UMEM until all duplicated sockets are
            // gone. This is the error/unwind cleanup, not a success result.
            let code = unsafe { edge_xsk_close(pointer.as_ptr()) };
            if code < 0 {
                eprintln!(
                    "native fixture cleanup failed: {}",
                    io::Error::from_raw_os_error(-code)
                );
            }
        }
    }
}

struct PinnedFd {
    // Fields drop in this order: close the duplicate, then release its UMEM
    // owner. Rc is cloned only during setup, never per packet.
    fd: OwnedFd,
    _fixture: Rc<Fixture>,
}

impl PinnedFd {
    fn new(fixture: &Rc<Fixture>, peer: bool) -> Result<Self> {
        let fd = if peer {
            fixture.peer_fd()
        } else {
            fixture.socket_fd()
        };
        // SAFETY: fd is borrowed from the live fixture; successful fcntl
        // creates an independent descriptor with one owner below.
        let duplicate = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error()).context("duplicate benchmark socket");
        }
        Ok(Self {
            // SAFETY: the successful fcntl result has no other Rust owner.
            fd: unsafe { OwnedFd::from_raw_fd(duplicate) },
            _fixture: fixture.clone(),
        })
    }
}

impl AsRawFd for PinnedFd {
    fn as_raw_fd(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Engine {
    Worker,
    Readable,
    ReadWrite,
}

impl Engine {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "worker" => Ok(Self::Worker),
            "readable" => Ok(Self::Readable),
            "read-write" => Ok(Self::ReadWrite),
            _ => bail!("unsupported engine {value}"),
        }
    }
}

async fn receive(fixture: &Fixture, peer: &AsyncFd<PinnedFd>, sequence: u64) -> Result<()> {
    loop {
        let mut guard = peer.readable().await?;
        if fixture.receive(sequence)? {
            return Ok(());
        }
        guard.clear_ready();
    }
}

#[derive(Default)]
struct RelayStats {
    waits: u64,
    empty_polls: u64,
    tx_backpressure: u64,
    measured_waits: u64,
}

async fn relay(
    fixture: &Fixture,
    io: Option<&AsyncFd<PinnedFd>>,
    worker: &mut Option<legacy::AfXdpWakeWorker>,
    warmup: u64,
    count: u64,
) -> Result<RelayStats> {
    // The guard borrows the separately owned registration. Keep it across
    // successful RX and partial drains, and clear only after observing empty.
    let mut ready: Option<tokio::io::unix::AsyncFdReadyGuard<'_, PinnedFd>> = None;
    let mut sequence = 0;
    let mut stats = RelayStats::default();
    while sequence < count {
        match fixture.echo(sequence)? {
            1 => sequence += 1,
            2 => {
                stats.tx_backpressure += 1;
                tokio::task::yield_now().await;
            }
            0 => {
                stats.empty_polls += 1;
                if let Some(mut guard) = ready.take() {
                    guard.clear_ready();
                }
                stats.waits += 1;
                if sequence >= warmup {
                    stats.measured_waits += 1;
                }
                if let Some(io) = io {
                    ready = Some(io.readable().await?);
                } else {
                    let worker = worker.as_mut().context("missing poll worker")?;
                    if worker.is_finished() {
                        worker.shutdown()?;
                        bail!("poll worker exited before transfer completed");
                    }
                    let rx = worker
                        .receiver_mut()
                        .as_mut()
                        .context("missing wake receiver")?;
                    ensure!(rx.recv().await.is_some(), "poll worker wake channel closed");
                }
            }
            other => bail!("invalid native echo result {other}"),
        }
    }
    Ok(stats)
}

struct ClientStats {
    elapsed: Duration,
    samples: Vec<u64>,
}

async fn client(
    fixture: &Fixture,
    peer: &AsyncFd<PinnedFd>,
    warmup: u64,
    count: u64,
) -> Result<ClientStats> {
    let mut samples = Vec::with_capacity(count as usize);
    for sequence in 0..warmup {
        fixture.send(sequence)?;
        receive(fixture, peer, sequence).await?;
    }
    let started = Instant::now();
    for sequence in warmup..warmup + count {
        let request = Instant::now();
        fixture.send(sequence)?;
        receive(fixture, peer, sequence).await?;
        samples.push(request.elapsed().as_nanos() as u64);
    }
    Ok(ClientStats {
        elapsed: started.elapsed(),
        samples,
    })
}

fn snapshot() -> Result<serde_json::Value> {
    let fds = std::fs::read_dir("/proc/self/fd")?
        .try_fold(0usize, |count, entry| entry.map(|_| count + 1))?;
    let mut threads = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let path = entry?.path();
        let name = std::fs::read_to_string(path.join("comm"))?;
        let status = std::fs::read_to_string(path.join("status"))?;
        let cpus = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .context("missing CPU affinity")?;
        threads.push(serde_json::json!({"name":name.trim(),"cpus":cpus.trim()}));
    }
    Ok(serde_json::json!({"fds":fds,"threads":threads}))
}

async fn drain_completions(
    fixture: &Fixture,
    received: u64,
    submitted: u64,
) -> Result<NativeStats> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        fixture.reclaim()?;
        let stats = fixture.stats();
        ensure!(
            stats.received == received && stats.submitted == submitted,
            "native byte/frame totals differ: {stats:?}"
        );
        if stats.completed == submitted {
            return Ok(stats);
        }
        ensure!(
            Instant::now() < deadline,
            "TX completion drain deadline: {stats:?}"
        );
        tokio::task::yield_now().await;
    }
}

struct Settings {
    rx: String,
    peer: String,
    engine: Engine,
    warmup: u64,
    count: u64,
    output: PathBuf,
    functional: bool,
}

impl Settings {
    fn parse() -> Result<Self> {
        let mut rx = None;
        let mut peer = None;
        let mut engine = None;
        let mut output = None;
        let mut warmup = 1000;
        let mut count = 50_000;
        let mut functional = false;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            if flag == "--functional" {
                functional = true;
                continue;
            }
            let value = args
                .next()
                .with_context(|| format!("missing value for {flag}"))?;
            match flag.as_str() {
                "--rx" => rx = Some(value),
                "--peer" => peer = Some(value),
                "--engine" => engine = Some(Engine::parse(&value)?),
                "--output" => output = Some(PathBuf::from(value)),
                "--warmup" => warmup = value.parse()?,
                "--roundtrips" => count = value.parse()?,
                _ => bail!("unknown argument {flag}"),
            }
        }
        ensure!(
            count > 0 && count <= 1_000_000 && warmup <= 1_000_000,
            "iteration count out of range"
        );
        Ok(Self {
            rx: rx.context("--rx required")?,
            peer: peer.context("--peer required")?,
            engine: engine.context("--engine required")?,
            output: output.context("--output required")?,
            warmup,
            count,
            functional,
        })
    }
}

async fn functional(
    fixture: &Fixture,
    io: &AsyncFd<PinnedFd>,
    peer: &AsyncFd<PinnedFd>,
) -> Result<serde_json::Value> {
    ensure!(
        tokio::time::timeout(Duration::from_millis(200), io.readable())
            .await
            .is_err(),
        "empty XSK unexpectedly readable"
    );
    for sequence in 0..8 {
        fixture.send(sequence)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.pending()? != 8 {
        ensure!(Instant::now() < deadline, "complete burst did not reach RX");
        tokio::task::yield_now().await;
    }
    let mut guard = tokio::time::timeout(Duration::from_secs(5), io.readable()).await??;
    ensure!(fixture.receive_only(0)?, "first burst frame unavailable");
    // Deliberate negative control: clearing before draining loses the edge.
    // No additional peer frame is sent while this check waits.
    guard.clear_ready();
    let early_clear_loses_edge = tokio::time::timeout(Duration::from_millis(200), io.readable())
        .await
        .is_err();
    ensure!(
        early_clear_loses_edge,
        "negative control received another RX event"
    );
    for sequence in 1..8 {
        ensure!(
            fixture.receive_only(sequence)?,
            "remaining burst frame missing"
        );
    }
    drain_completions(fixture, 8, 0).await?;
    ensure!(fixture.echo(8)? == 0, "RX must be empty after burst");
    ensure!(
        tokio::time::timeout(Duration::from_millis(200), io.readable())
            .await
            .is_err(),
        "drained XSK remained readable"
    );
    // Positive control: preserve cached readiness between partial drains,
    // including a runtime yield, until a subsequent receive observes empty.
    for sequence in 8..16 {
        fixture.send(sequence)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.pending()? != 8 {
        ensure!(Instant::now() < deadline, "positive burst did not reach RX");
        tokio::task::yield_now().await;
    }
    for sequence in 8..16 {
        let mut guard = tokio::time::timeout(Duration::from_secs(5), io.readable()).await??;
        ensure!(
            fixture.echo(sequence)? == 1,
            "retained-readiness burst frame missing"
        );
        if sequence == 15 {
            ensure!(fixture.echo(16)? == 0, "positive-control RX must be empty");
            guard.clear_ready();
        }
        tokio::task::yield_now().await;
    }
    for sequence in 8..16 {
        receive(fixture, peer, sequence).await?;
    }
    drain_completions(fixture, 16, 8).await?;
    ensure!(
        tokio::time::timeout(Duration::from_millis(200), io.readable())
            .await
            .is_err(),
        "positive-control idle unexpectedly ready"
    );
    Ok(
        serde_json::json!({"early_clear_loses_edge":early_clear_loses_edge,"negative_control_rx_only":8,"retained_readiness_burst_passed":true,"positive_control_echoes":8,"idle_after_drain":true}),
    )
}

async fn run(settings: Settings) -> Result<serde_json::Value> {
    let fixture = Fixture::open(&settings.rx, &settings.peer)?;
    let peer = AsyncFd::with_interest(PinnedFd::new(&fixture, true)?, Interest::READABLE)?;
    let io = if settings.engine == Engine::Worker {
        None
    } else {
        let interest = if settings.engine == Engine::Readable {
            Interest::READABLE
        } else {
            Interest::READABLE | Interest::WRITABLE
        };
        Some(AsyncFd::with_interest(
            PinnedFd::new(&fixture, false)?,
            interest,
        )?)
    };
    let mut worker = if settings.engine == Engine::Worker {
        Some(legacy::AfXdpWakeWorker::start(
            fixture.socket_fd(),
            "xsk-wake-bench".into(),
        )?)
    } else {
        None
    };
    let result = if settings.functional {
        let io = io
            .as_ref()
            .context("functional negative control requires a readiness engine")?;
        functional(&fixture, io, &peer)
            .await
            .with_context(|| format!("functional counters: {:?}", fixture.stats()))?
    } else {
        let (client, relay) = tokio::time::timeout(Duration::from_secs(120), async {
            tokio::try_join!(
                client(&fixture, &peer, settings.warmup, settings.count),
                relay(
                    &fixture,
                    io.as_ref(),
                    &mut worker,
                    settings.warmup,
                    settings.warmup + settings.count
                )
            )
        })
        .await
        .with_context(|| format!("transfer timeout counters: {:?}", fixture.stats()))?
        .with_context(|| format!("transfer error counters: {:?}", fixture.stats()))?;
        let resources = snapshot()?;
        let native = drain_completions(
            &fixture,
            settings.warmup + settings.count,
            settings.warmup + settings.count,
        )
        .await?;
        ensure!(relay.waits > 0, "wake mechanism was not exercised");
        ensure!(
            relay.measured_waits >= settings.count - 1,
            "timed requests bypassed the wake mechanism"
        );
        let mut sorted = client.samples.clone();
        sorted.sort_unstable();
        serde_json::json!({"scope":"single-process AF_PACKET client + copy-mode XSK echo; wake component only","engine":format!("{:?}",settings.engine),"warmup_roundtrips":settings.warmup,"measured_roundtrips":settings.count,"wire_bytes_per_frame":128,"elapsed_seconds":client.elapsed.as_secs_f64(),"mean_roundtrip_ns":client.elapsed.as_nanos() as f64/settings.count as f64,"roundtrips_per_second":settings.count as f64/client.elapsed.as_secs_f64(),"rtt_p50_ns":sorted[sorted.len()/2],"rtt_p99_ns":sorted[(sorted.len()*99).div_ceil(100)-1],"latency_samples_ns":client.samples,"wake_waits":relay.waits,"measured_wake_waits":relay.measured_waits,"empty_rx_polls":relay.empty_polls,"tx_backpressure":relay.tx_backpressure,"resources":resources,"native":{"received":native.received,"submitted":native.submitted,"completed":native.completed,"fill_kicks":native.fill_kicks}})
    };
    if let Some(worker) = worker.as_mut() {
        worker.shutdown()?;
    }
    drop(worker);
    drop(io);
    drop(peer);
    Rc::try_unwrap(fixture)
        .map_err(|_| anyhow::anyhow!("fixture still has socket owners"))?
        .close()?;
    Ok(result)
}

fn main() -> Result<()> {
    let settings = Settings::parse()?;
    let output = settings.output.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(settings));
    match result {
        Ok(mut value) => {
            value["passed"] = true.into();
            std::fs::write(output, serde_json::to_vec_pretty(&value)?)?;
            Ok(())
        }
        Err(error) => {
            std::fs::write(
                output,
                serde_json::to_vec_pretty(
                    &serde_json::json!({"passed":false,"error":format!("{error:#}")}),
                )?,
            )?;
            Err(error)
        }
    }
}
