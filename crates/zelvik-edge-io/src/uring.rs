//! Experimental shared io_uring splice worker. Disabled by default.
//!
//! A worker owns its ring, sockets, pipes, observers and every submitted
//! operation through its CQE. Connections share submission batches. Dropping
//! a transfer requests cancellation; it does not abandon kernel operations.

use crate::splice::{Direction, Observer, Operation, Pipe, PipeCapacity, TransferError};
use io_uring::{IoUring, Probe, opcode, squeue, types};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::TcpStream;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

const CANCEL_BIT: u64 = 1 << 63;

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub ring_entries: u32,
    pub max_connections: usize,
    pub pipe_capacity_hint: usize,
    /// SINGLE_ISSUER + DEFER_TASKRUN. Requires Linux 6.1 or newer.
    pub defer_taskrun: bool,
    /// Kernel worker limits [bounded, unbounded]. None explicitly retains
    /// kernel defaults for comparison. Both configured limits must be nonzero.
    pub io_wq_max_workers: Option<[u32; 2]>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ring_entries: 256,
            max_connections: 1024,
            pipe_capacity_hint: 256 * 1024,
            defer_taskrun: true,
            io_wq_max_workers: Some([2, 2]),
        }
    }
}

#[derive(Default)]
pub struct NoopObserver;

impl Observer for NoopObserver {
    fn transferred(&mut self, _: usize) {}
}

#[derive(Debug)]
pub enum EndReason {
    Clean,
    Cancelled,
    IdleTimeout(Direction),
    Failed(TransferError),
}

#[derive(Debug)]
pub struct TransferOutcome {
    pub forward_bytes: u64,
    pub reverse_bytes: u64,
    pub pipe_capacities: [PipeCapacity; 2],
    pub reason: EndReason,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    pub accepted: u64,
    pub rejected: u64,
    pub completed: u64,
    pub abandoned_receivers: u64,
    pub submit_calls: u64,
    pub completions: u64,
    /// Limits read back on the issuer before shutdown. Zero means the
    /// kernel has not initialized an io-wq for this issuer.
    pub io_wq_limits: [u32; 2],
    /// Number of CPUs explicitly permitted for kernel workers at startup.
    pub io_wq_cpus: u32,
}

struct Shared {
    wake: OwnedFd,
    stopping: AtomicBool,
    next_id: AtomicU64,
}

impl Shared {
    fn notify(&self) -> io::Result<()> {
        let value = 1u64;
        loop {
            // SAFETY: wake is an owned eventfd; value supplies exactly eight
            // readable bytes and remains alive through the syscall.
            let written =
                unsafe { libc::write(self.wake.as_raw_fd(), (&value as *const u64).cast(), 8) };
            if written == 8 {
                return Ok(());
            }
            if written >= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short eventfd write",
                ));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                // A saturated eventfd is already readable. One wake is
                // enough: the worker drains the queue and cancellation flags.
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }
}

struct Start<O> {
    id: u64,
    sockets: [TcpStream; 2],
    observers: [O; 2],
    cancel: Arc<AtomicBool>,
    response: oneshot::Sender<io::Result<TransferOutcome>>,
}

pub struct Handle<O: Observer + Send + 'static = NoopObserver> {
    sender: mpsc::Sender<Start<O>>,
    shared: Arc<Shared>,
}

impl<O: Observer + Send + 'static> Clone for Handle<O> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            shared: self.shared.clone(),
        }
    }
}

impl<O: Observer + Send + 'static> Handle<O> {
    pub async fn start(
        &self,
        first: TcpStream,
        second: TcpStream,
        forward: O,
        reverse: O,
    ) -> io::Result<Transfer> {
        if self.shared.stopping.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "io_uring worker stopped",
            ));
        }
        first.set_nonblocking(true)?;
        second.set_nonblocking(true)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        if id >= CANCEL_BIT / 2 {
            return Err(io::Error::other("io_uring connection IDs exhausted"));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (response, receiver) = oneshot::channel();
        let transfer = Transfer {
            receiver,
            cancel: cancel.clone(),
            shared: self.shared.clone(),
            finished: false,
        };
        self.sender
            .send(Start {
                id,
                sockets: [first, second],
                observers: [forward, reverse],
                cancel,
                response,
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::BrokenPipe, "io_uring command queue closed")
            })?;
        self.shared.notify()?;
        Ok(transfer)
    }
}

pub struct Transfer {
    receiver: oneshot::Receiver<io::Result<TransferOutcome>>,
    cancel: Arc<AtomicBool>,
    shared: Arc<Shared>,
    finished: bool,
}

impl Transfer {
    /// Request cancellation, then await this same transfer to observe all
    /// destination writes acknowledged by CQEs, including completion races.
    pub fn cancel(&self) -> io::Result<()> {
        self.cancel.store(true, Ordering::Release);
        self.shared.notify()
    }
}

impl Future for Transfer {
    type Output = io::Result<TransferOutcome>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.receiver).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                this.finished = true;
                Poll::Ready(result.unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "io_uring worker exited before completion",
                    ))
                }))
            }
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.cancel()
        {
            eprintln!("io_uring transfer cancellation wake failed: {error}");
        }
    }
}

pub struct Driver<O: Observer + Send + 'static = NoopObserver> {
    handle: Handle<O>,
    join: Option<JoinHandle<io::Result<Report>>>,
}

impl<O: Observer + Send + 'static> Driver<O> {
    pub fn new(config: Config) -> io::Result<Self> {
        if config.max_connections == 0
            || config.max_connections > tokio::sync::Semaphore::MAX_PERMITS
            || config.max_connections > u32::MAX as usize / 6
            || config.pipe_capacity_hint == 0
            || config.pipe_capacity_hint > i32::MAX as usize
            || config.io_wq_max_workers.is_some_and(|limits| {
                limits
                    .iter()
                    .any(|&limit| limit == 0 || limit > i32::MAX as u32)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid io_uring worker capacity",
            ));
        }
        // SAFETY: eventfd returns a new owned descriptor or a negative errno.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful eventfd result has no other Rust owner.
        let wake = unsafe { OwnedFd::from_raw_fd(fd) };
        let shared = Arc::new(Shared {
            wake,
            stopping: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        });
        let (sender, receiver) = mpsc::channel(config.max_connections);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let worker_shared = shared.clone();
        // The ring must be created on its issuer thread when SINGLE_ISSUER
        // is enabled. Never create it on the Tokio caller and move it later.
        let join = std::thread::Builder::new()
            .name("edge-uring".into())
            .spawn(move || {
                let mut worker = match Worker::new(config, receiver, worker_shared.clone()) {
                    Ok(worker) => worker,
                    Err(error) => {
                        let copy = copy_error(&error);
                        if ready_tx.send(Err(copy)).is_err() {
                            return Err(io::Error::other(format!(
                                "io_uring startup receiver closed: {error}"
                            )));
                        }
                        return Err(error);
                    }
                };
                ready_tx.send(Ok(())).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "io_uring startup receiver closed",
                    )
                })?;
                let result = worker.run();
                worker_shared.stopping.store(true, Ordering::Release);
                result
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                handle: Handle { sender, shared },
                join: Some(join),
            }),
            Ok(Err(error)) => {
                if join.join().is_err() {
                    return Err(io::Error::other("io_uring startup worker panicked"));
                }
                Err(error)
            }
            Err(_) => match join.join() {
                Ok(Err(error)) => Err(error),
                Ok(Ok(_)) => Err(io::Error::other("io_uring worker exited during startup")),
                Err(_) => Err(io::Error::other("io_uring startup worker panicked")),
            },
        }
    }

    pub fn handle(&self) -> Handle<O> {
        self.handle.clone()
    }

    /// Stop admissions, cancel/drain all submitted operations, and join the
    /// owned worker. None means a previous call already joined it.
    pub fn shutdown(&mut self) -> io::Result<Option<Report>> {
        let Some(join) = self.join.take() else {
            return Ok(None);
        };
        self.handle.shared.stopping.store(true, Ordering::Release);
        let wake = self.handle.shared.notify();
        let result = join
            .join()
            .map_err(|_| io::Error::other("io_uring worker panicked"))?;
        wake?;
        result.map(Some)
    }
}

impl<O: Observer + Send + 'static> Drop for Driver<O> {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("io_uring worker shutdown failed: {error}");
        }
    }
}

fn copy_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(errno) => io::Error::from_raw_os_error(errno),
        None => io::Error::new(error.kind(), error.to_string()),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Read,
    Write,
    PollRead,
    PollWrite,
    Done,
}

impl Phase {
    fn operation(self) -> Operation {
        match self {
            Self::Read | Self::PollRead => Operation::Read,
            _ => Operation::Write,
        }
    }
}

struct Lane<O> {
    pipe: Pipe,
    observer: O,
    total: u64,
    phase: Phase,
    in_flight: bool,
    cancel_in_flight: bool,
    read_started: bool,
    deadline: Option<Instant>,
}

impl<O> Lane<O> {
    fn new(pipe: Pipe, observer: O) -> Self {
        Self {
            pipe,
            observer,
            total: 0,
            phase: Phase::Read,
            in_flight: false,
            cancel_in_flight: false,
            read_started: false,
            deadline: None,
        }
    }
}

struct Connection<O> {
    slot: u32,
    sockets: [TcpStream; 2],
    lanes: [Lane<O>; 2],
    cancel: Arc<AtomicBool>,
    response: oneshot::Sender<io::Result<TransferOutcome>>,
    terminal: Option<EndReason>,
}

impl<O> Connection<O> {
    fn finished(&self) -> bool {
        self.lanes
            .iter()
            .all(|lane| lane.phase == Phase::Done && !lane.in_flight && !lane.cancel_in_flight)
    }
}

struct Worker<O> {
    // Drop the ring before descriptor owners even on a fatal worker error.
    // Normal exits additionally drain target and cancellation CQEs first.
    ring: IoUring,
    connections: HashMap<u64, Connection<O>>,
    receiver: mpsc::Receiver<Start<O>>,
    shared: Arc<Shared>,
    config: Config,
    pending: VecDeque<squeue::Entry>,
    free_slots: Vec<u32>,
    deadlines: BTreeSet<(Instant, u64)>,
    stopping: bool,
    wake_pending: bool,
    report: Report,
}

impl<O: Observer + Send + 'static> Worker<O> {
    fn new(
        config: Config,
        receiver: mpsc::Receiver<Start<O>>,
        shared: Arc<Shared>,
    ) -> io::Result<Self> {
        let mut builder = IoUring::builder();
        if config.defer_taskrun {
            builder.setup_single_issuer().setup_defer_taskrun();
        }
        let ring = builder.build(config.ring_entries)?;
        // Linux 6.8 initializes io-wq with cpu_possible_mask, independently
        // of this thread's sched affinity. Bind workers explicitly before
        // admitting I/O, so they obey the same CPU budget as their issuer.
        // SAFETY: cpu_set_t is fully initialized and writable for the syscall.
        let mut affinity: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&affinity), &mut affinity) }
            < 0
        {
            return Err(io::Error::last_os_error());
        }
        ring.submitter().register_iowq_aff(&affinity)?;
        if let Some(mut limits) = config.io_wq_max_workers {
            ring.submitter().register_iowq_max_workers(&mut limits)?;
        }
        // SAFETY: affinity was initialized above and populated by the kernel.
        let io_wq_cpus = unsafe { libc::CPU_COUNT(&affinity) } as u32;
        let mut probe = Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        for code in [
            opcode::Splice::CODE,
            opcode::PollAdd::CODE,
            opcode::AsyncCancel::CODE,
        ] {
            if !probe.is_supported(code) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("io_uring opcode {code} unsupported"),
                ));
            }
        }
        if !ring.params().is_feature_nodrop() || !ring.params().is_feature_ext_arg() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "io_uring requires NODROP and EXT_ARG",
            ));
        }
        // Pin files for io-wq's deferred splice input lookup. A fatal ring
        // close must not let a raw input fd name an unrelated new socket.
        ring.submitter()
            .register_files_sparse((config.max_connections * 6) as u32)?;
        Ok(Self {
            ring,
            connections: HashMap::new(),
            receiver,
            shared,
            config,
            pending: VecDeque::new(),
            free_slots: (0..config.max_connections as u32).rev().collect(),
            deadlines: BTreeSet::new(),
            stopping: false,
            wake_pending: false,
            report: Report {
                io_wq_cpus,
                ..Report::default()
            },
        })
    }

    fn arm_wake(&mut self) {
        self.pending.push_back(
            opcode::PollAdd::new(types::Fd(self.shared.wake.as_raw_fd()), libc::POLLIN as u32)
                .build()
                .user_data(0),
        );
        self.wake_pending = true;
    }

    fn run(&mut self) -> io::Result<Report> {
        self.arm_wake();
        let mut completions = Vec::with_capacity(self.ring.params().cq_entries() as usize);
        loop {
            completions.clear();
            completions.extend(
                self.ring
                    .completion()
                    .map(|cqe| (cqe.user_data(), cqe.result())),
            );
            self.report.completions += completions.len() as u64;
            for &(token, result) in &completions {
                self.complete(token, result)?;
            }
            self.expire()?;
            if self.shared.stopping.load(Ordering::Acquire) && !self.stopping {
                self.stop()?;
            }
            if self.stopping
                && self.connections.is_empty()
                && !self.wake_pending
                && self.pending.is_empty()
            {
                // Query only after I/O: before io-wq is initialized the
                // registration API returns zero rather than effective limits.
                let mut limits = [0, 0];
                self.ring
                    .submitter()
                    .register_iowq_max_workers(&mut limits)?;
                self.report.io_wq_limits = limits;
                return Ok(self.report);
            }
            {
                let mut sq = self.ring.submission();
                if sq.dropped() != 0 {
                    return Err(io::Error::other("io_uring dropped submissions"));
                }
                while let Some(entry) = self.pending.front() {
                    // SAFETY: sockets/pipes remain in connections until both
                    // target and cancel CQEs. eventfd remains shared-owned.
                    // SQEs contain no pointers to movable userspace buffers.
                    if unsafe { sq.push(entry) }.is_err() {
                        break;
                    }
                    self.pending.pop_front();
                }
            }
            self.report.submit_calls += 1;
            // Flush a full SQ without waiting while there is a backlog.
            // Waiting on idle polls here would strand ready operations that
            // have not fitted into a small ring yet.
            let result = if !self.pending.is_empty() {
                self.ring.submit()
            } else if let Some(&(deadline, _)) = self.deadlines.first() {
                let timeout =
                    types::Timespec::from(deadline.saturating_duration_since(Instant::now()));
                let args = types::SubmitArgs::new().timespec(&timeout);
                self.ring.submitter().submit_with_args(1, &args)
            } else {
                self.ring.submit_and_wait(1)
            };
            match result {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if error.raw_os_error() == Some(libc::ETIME) && !self.deadlines.is_empty() => {}
                // NODROP CQ pressure requires reaping before retrying. The
                // already published SQ entries remain in the ring.
                Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn drain_wake(&self) -> io::Result<()> {
        loop {
            let mut value = 0u64;
            // SAFETY: wake is a live eventfd and value is an eight-byte
            // writable destination kept alive through the syscall.
            let count = unsafe {
                libc::read(
                    self.shared.wake.as_raw_fd(),
                    (&mut value as *mut u64).cast(),
                    8,
                )
            };
            if count == 8 {
                continue;
            }
            if count >= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "short eventfd read",
                ));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(error),
            }
        }
    }

    fn control(&mut self) -> io::Result<()> {
        while let Ok(start) = self.receiver.try_recv() {
            if self.stopping || self.connections.len() >= self.config.max_connections {
                self.report.rejected += 1;
                self.respond(
                    start.response,
                    Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "io_uring worker at capacity or stopped",
                    )),
                );
                continue;
            }
            let pipes = Pipe::new(self.config.pipe_capacity_hint).and_then(|first| {
                Pipe::new(self.config.pipe_capacity_hint).map(|second| [first, second])
            });
            let [first, second] = match pipes {
                Ok(pipes) => pipes,
                Err(error) => {
                    self.report.rejected += 1;
                    self.respond(start.response, Err(error));
                    continue;
                }
            };
            let [forward, reverse] = start.observers;
            let slot = self
                .free_slots
                .pop()
                .ok_or_else(|| io::Error::other("io_uring file slots exhausted"))?;
            let files = [
                start.sockets[0].as_raw_fd(),
                start.sockets[1].as_raw_fd(),
                first.read.as_raw_fd(),
                first.write.as_raw_fd(),
                second.read.as_raw_fd(),
                second.write.as_raw_fd(),
            ];
            if self
                .ring
                .submitter()
                .register_files_update(slot * 6, &files)?
                != files.len()
            {
                return Err(io::Error::other("short io_uring file registration"));
            }
            self.connections.insert(
                start.id,
                Connection {
                    slot,
                    sockets: start.sockets,
                    lanes: [Lane::new(first, forward), Lane::new(second, reverse)],
                    cancel: start.cancel,
                    response: start.response,
                    terminal: None,
                },
            );
            self.report.accepted += 1;
            self.schedule(start.id, 0, Phase::Read)?;
            self.schedule(start.id, 1, Phase::Read)?;
        }
        let cancelled: Vec<_> = self
            .connections
            .iter()
            .filter(|(_, c)| c.cancel.load(Ordering::Acquire) || c.response.is_closed())
            .map(|(&id, _)| id)
            .collect();
        for id in cancelled {
            self.terminate(id, EndReason::Cancelled)?;
        }
        Ok(())
    }

    fn schedule(&mut self, id: u64, index: usize, phase: Phase) -> io::Result<()> {
        let token = id * 2 + index as u64;
        let connection = self
            .connections
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("missing scheduled io_uring connection"))?;
        let source = connection.slot * 6 + index as u32;
        let destination = connection.slot * 6 + (1 - index) as u32;
        let pipe_read = connection.slot * 6 + 2 + index as u32 * 2;
        let pipe_write = pipe_read + 1;
        let lane = &mut connection.lanes[index];
        if connection.terminal.is_some() {
            lane.phase = Phase::Done;
            return Ok(());
        }
        if phase == Phase::Read && !lane.read_started {
            lane.read_started = true;
            if let Some(timeout) = lane.observer.read_idle_timeout() {
                let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "io_uring read deadline overflow",
                    )
                })?;
                lane.deadline = Some(deadline);
                self.deadlines.insert((deadline, token));
            }
        }
        let entry = match phase {
            Phase::Read => opcode::Splice::new(
                types::Fixed(source),
                -1,
                types::Fixed(pipe_write),
                -1,
                lane.pipe.capacity.actual as u32,
            )
            .flags(libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK)
            .build(),
            Phase::Write => opcode::Splice::new(
                types::Fixed(pipe_read),
                -1,
                types::Fixed(destination),
                -1,
                lane.pipe.pending as u32,
            )
            .flags(libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK)
            .build(),
            Phase::PollRead => {
                opcode::PollAdd::new(types::Fixed(source), libc::POLLIN as u32).build()
            }
            Phase::PollWrite => {
                opcode::PollAdd::new(types::Fixed(destination), libc::POLLOUT as u32).build()
            }
            Phase::Done => {
                return Err(io::Error::other(
                    "cannot schedule completed io_uring direction",
                ));
            }
        };
        lane.phase = phase;
        lane.in_flight = true;
        self.pending.push_back(entry.user_data(token));
        Ok(())
    }

    fn clear_deadline(&mut self, id: u64, index: usize) {
        if let Some(connection) = self.connections.get_mut(&id) {
            let lane = &mut connection.lanes[index];
            if let Some(deadline) = lane.deadline.take() {
                self.deadlines.remove(&(deadline, id * 2 + index as u64));
            }
            lane.read_started = false;
        }
    }

    fn complete(&mut self, token: u64, result: i32) -> io::Result<()> {
        if token == 0 {
            self.wake_pending = false;
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result));
            }
            self.drain_wake()?;
            self.control()?;
            if !self.shared.stopping.load(Ordering::Acquire) {
                self.arm_wake();
            }
            return Ok(());
        }
        let data_token = token & !CANCEL_BIT;
        let id = data_token / 2;
        let index = (data_token % 2) as usize;
        let connection = self
            .connections
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("stale io_uring completion"))?;
        let lane = &mut connection.lanes[index];
        if token & CANCEL_BIT != 0 {
            lane.cancel_in_flight = false;
            if ![0, -libc::ENOENT, -libc::EALREADY].contains(&result) {
                return Err(io::Error::other(format!(
                    "io_uring cancellation failed: {result}"
                )));
            }
        } else {
            let phase = lane.phase;
            lane.in_flight = false;
            // Always account accepted destination writes before observing
            // cancellation. A successful CQE can race with the cancel SQE.
            if phase == Phase::Write && result > 0 {
                lane.pipe.pending -= result as usize;
                lane.total += result as u64;
                lane.observer.transferred(result as usize);
            }
            if connection.terminal.is_some() {
                lane.phase = Phase::Done;
            } else if result < 0 {
                let error = io::Error::from_raw_os_error(-result);
                if error.kind() == io::ErrorKind::WouldBlock
                    && matches!(phase, Phase::Read | Phase::Write)
                {
                    self.schedule(
                        id,
                        index,
                        if phase == Phase::Read {
                            Phase::PollRead
                        } else {
                            Phase::PollWrite
                        },
                    )?;
                } else if error.kind() == io::ErrorKind::Interrupted {
                    self.schedule(id, index, phase)?;
                } else {
                    self.fail(id, index, phase.operation(), error)?;
                }
            } else {
                match phase {
                    Phase::Read => {
                        self.clear_deadline(id, index);
                        if result == 0 {
                            let connection = self
                                .connections
                                .get_mut(&id)
                                .ok_or_else(|| io::Error::other("missing EOF connection"))?;
                            // SAFETY: the connection owns the destination
                            // socket; SHUT_WR preserves its reverse reads.
                            if unsafe {
                                libc::shutdown(
                                    connection.sockets[1 - index].as_raw_fd(),
                                    libc::SHUT_WR,
                                )
                            } < 0
                            {
                                self.fail(id, index, Operation::Write, io::Error::last_os_error())?;
                            } else {
                                connection.lanes[index].phase = Phase::Done;
                            }
                        } else {
                            self.connections
                                .get_mut(&id)
                                .ok_or_else(|| io::Error::other("missing read connection"))?
                                .lanes[index]
                                .pipe
                                .pending = result as usize;
                            self.schedule(id, index, Phase::Write)?;
                        }
                    }
                    Phase::Write => {
                        if result == 0 {
                            self.fail(
                                id,
                                index,
                                Operation::Write,
                                io::Error::new(
                                    io::ErrorKind::WriteZero,
                                    "io_uring splice wrote zero bytes",
                                ),
                            )?;
                        } else {
                            let pending = self
                                .connections
                                .get(&id)
                                .ok_or_else(|| io::Error::other("missing write connection"))?
                                .lanes[index]
                                .pipe
                                .pending;
                            self.schedule(
                                id,
                                index,
                                if pending == 0 {
                                    Phase::Read
                                } else {
                                    Phase::Write
                                },
                            )?;
                        }
                    }
                    Phase::PollRead | Phase::PollWrite => {
                        if result == 0 {
                            self.fail(
                                id,
                                index,
                                phase.operation(),
                                io::Error::other("empty io_uring poll completion"),
                            )?;
                        } else {
                            self.schedule(
                                id,
                                index,
                                if phase == Phase::PollRead {
                                    Phase::Read
                                } else {
                                    Phase::Write
                                },
                            )?;
                        }
                    }
                    Phase::Done => return Err(io::Error::other("duplicate io_uring completion")),
                }
            }
        }
        self.finish(id)
    }

    fn fail(
        &mut self,
        id: u64,
        index: usize,
        operation: Operation,
        source: io::Error,
    ) -> io::Result<()> {
        self.terminate(
            id,
            EndReason::Failed(TransferError {
                direction: direction(index),
                operation,
                source,
            }),
        )
    }

    fn terminate(&mut self, id: u64, reason: EndReason) -> io::Result<()> {
        let connection = self
            .connections
            .get_mut(&id)
            .ok_or_else(|| io::Error::other("missing cancelled io_uring connection"))?;
        if connection.terminal.is_none() {
            connection.terminal = Some(reason);
        }
        for (index, lane) in connection.lanes.iter_mut().enumerate() {
            let token = id * 2 + index as u64;
            if let Some(deadline) = lane.deadline.take() {
                self.deadlines.remove(&(deadline, token));
            }
            if lane.in_flight && !lane.cancel_in_flight {
                lane.cancel_in_flight = true;
                self.pending.push_back(
                    opcode::AsyncCancel::new(token)
                        .build()
                        .user_data(token | CANCEL_BIT),
                );
            } else if !lane.in_flight {
                lane.phase = Phase::Done;
            }
        }
        self.finish(id)
    }

    fn expire(&mut self) -> io::Result<()> {
        while let Some(&(deadline, token)) = self.deadlines.first() {
            if deadline > Instant::now() {
                break;
            }
            self.terminate(
                token / 2,
                EndReason::IdleTimeout(direction((token % 2) as usize)),
            )?;
        }
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        self.stopping = true;
        self.receiver.close();
        self.control()?;
        let ids: Vec<_> = self.connections.keys().copied().collect();
        for id in ids {
            self.terminate(id, EndReason::Cancelled)?;
        }
        Ok(())
    }

    fn finish(&mut self, id: u64) -> io::Result<()> {
        if !self.connections.get(&id).is_some_and(Connection::finished) {
            return Ok(());
        }
        let Some(connection) = self.connections.remove(&id) else {
            return Ok(());
        };
        let reason = connection.terminal.unwrap_or(EndReason::Clean);
        let outcome = TransferOutcome {
            forward_bytes: connection.lanes[0].total,
            reverse_bytes: connection.lanes[1].total,
            pipe_capacities: [
                connection.lanes[0].pipe.capacity,
                connection.lanes[1].pipe.capacity,
            ],
            reason,
        };
        if self
            .ring
            .submitter()
            .register_files_update(connection.slot * 6, &[-1; 6])?
            != 6
        {
            return Err(io::Error::other("short io_uring file deregistration"));
        }
        self.free_slots.push(connection.slot);
        // Flush observers and release sockets/pipes before waking the caller.
        drop(connection.lanes);
        drop(connection.sockets);
        self.report.completed += 1;
        self.respond(connection.response, Ok(outcome));
        Ok(())
    }

    fn respond(
        &mut self,
        sender: oneshot::Sender<io::Result<TransferOutcome>>,
        result: io::Result<TransferOutcome>,
    ) {
        if sender.send(result).is_err() {
            // A dropped transfer intentionally has no receiver. Resource
            // cleanup still completed; retain the count in the worker report.
            self.report.abandoned_receivers += 1;
        }
    }
}

fn direction(index: usize) -> Direction {
    if index == 0 {
        Direction::Forward
    } else {
        Direction::Reverse
    }
}
