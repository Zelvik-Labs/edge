//! Nonblocking TCP splice driven by Tokio socket readiness.
//!
//! Each direction owns one pipe. No blocking worker or duplicated socket is
//! retained. A clean EOF half-closes its destination and lets the reverse
//! direction finish. Dropping the transfer cancels waits and closes its pipes.

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::time::Duration;
use tokio::io::Interest;
use tokio::net::TcpStream;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Reverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Read,
    Write,
}

#[derive(Debug)]
pub struct TransferError {
    pub direction: Direction,
    pub operation: Operation,
    pub source: io::Error,
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} {:?}: {}",
            self.direction, self.operation, self.source
        )
    }
}

impl std::error::Error for TransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Called after bytes are accepted by the destination socket. An observer's
/// Drop can flush accounting on EOF, error, cancellation or task abortion.
pub trait Observer {
    fn transferred(&mut self, bytes: usize);

    /// Resampled before each read; None keeps long-lived SNI streams untimed.
    fn read_idle_timeout(&self) -> Option<Duration> {
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PipeCapacity {
    pub requested: usize,
    pub actual: usize,
    /// Linux may deny a resize while the original pipe remains usable.
    pub resize_errno: Option<i32>,
}

pub(super) struct Pipe {
    pub(super) read: OwnedFd,
    pub(super) write: OwnedFd,
    pub(super) pending: usize,
    pub(super) capacity: PipeCapacity,
}

impl Pipe {
    pub(super) fn new(capacity_hint: usize) -> io::Result<Self> {
        let requested = libc::c_int::try_from(capacity_hint)
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid pipe capacity hint")
            })?;
        let mut fds = [0; 2];
        // SAFETY: fds points to two valid integers; successful pipe2 transfers
        // ownership of two distinct descriptors to the OwnedFd values below.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 succeeded and neither descriptor has another owner.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // Capacity is a hint, matching the previous kernel splice path. Keep
        // the actual capacity and denied-resize errno visible to the caller.
        // SAFETY: read owns a live pipe and requested is a positive c_int.
        let resized = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_SETPIPE_SZ, requested) };
        let resize_errno = (resized < 0)
            .then(|| io::Error::last_os_error().raw_os_error())
            .flatten();
        // SAFETY: read is an owned live pipe descriptor; this command takes no
        // pointer argument and returns the actual pipe capacity.
        let actual = unsafe { libc::fcntl(read.as_raw_fd(), libc::F_GETPIPE_SZ) };
        if actual < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            read,
            write,
            pending: 0,
            capacity: PipeCapacity {
                requested: capacity_hint,
                actual: actual as usize,
                resize_errno,
            },
        })
    }

    async fn read_from(&mut self, socket: &TcpStream) -> io::Result<usize> {
        debug_assert_eq!(self.pending, 0);
        let count = socket
            .async_io(Interest::READABLE, || {
                splice_once(
                    socket.as_raw_fd(),
                    self.write.as_raw_fd(),
                    self.capacity.actual,
                )
            })
            .await?;
        self.pending = count;
        Ok(count)
    }

    async fn write_to(&mut self, socket: &TcpStream) -> io::Result<usize> {
        debug_assert_ne!(self.pending, 0);
        let count = socket
            .async_io(Interest::WRITABLE, || {
                splice_once(self.read.as_raw_fd(), socket.as_raw_fd(), self.pending)
            })
            .await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "splice wrote zero bytes",
            ));
        }
        self.pending -= count;
        Ok(count)
    }
}

/// Allocate both pipes before any input is consumed. Callers can use a
/// different engine if preparation fails, without replaying consumed bytes.
pub struct PreparedSplice {
    forward: Pipe,
    reverse: Pipe,
}

impl PreparedSplice {
    pub fn new(capacity_hint: usize) -> io::Result<Self> {
        Ok(Self {
            forward: Pipe::new(capacity_hint)?,
            reverse: Pipe::new(capacity_hint)?,
        })
    }

    pub fn capacities(&self) -> [PipeCapacity; 2] {
        [self.forward.capacity, self.reverse.capacity]
    }

    pub async fn run<F: Observer, R: Observer>(
        self,
        first: &TcpStream,
        second: &TcpStream,
        forward: F,
        reverse: R,
    ) -> Result<(), TransferError> {
        let forward = copy_direction(first, second, self.forward, forward);
        let reverse = copy_direction(second, first, self.reverse, reverse);
        tokio::pin!(forward, reverse);
        tokio::select! {
            result = &mut forward => {
                result.map_err(|e| e.with_direction(Direction::Forward))?;
                reverse.await.map_err(|e| e.with_direction(Direction::Reverse))
            }
            result = &mut reverse => {
                result.map_err(|e| e.with_direction(Direction::Reverse))?;
                forward.await.map_err(|e| e.with_direction(Direction::Forward))
            }
        }
    }
}

struct DirectionError {
    operation: Operation,
    source: io::Error,
}

impl DirectionError {
    fn with_direction(self, direction: Direction) -> TransferError {
        TransferError {
            direction,
            operation: self.operation,
            source: self.source,
        }
    }
}

async fn copy_direction<O: Observer>(
    source: &TcpStream,
    destination: &TcpStream,
    mut pipe: Pipe,
    mut observer: O,
) -> Result<(), DirectionError> {
    loop {
        let read = if let Some(timeout) = observer.read_idle_timeout() {
            match tokio::time::timeout(timeout, pipe.read_from(source)).await {
                Ok(result) => result,
                Err(_) => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "splice read idle timeout",
                )),
            }
        } else {
            pipe.read_from(source).await
        };
        let count = read.map_err(|source| DirectionError {
            operation: Operation::Read,
            source,
        })?;
        if count == 0 {
            // SAFETY: the borrowed TcpStream owns a live socket. SHUT_WR
            // propagates FIN without interrupting the reverse read direction.
            if unsafe { libc::shutdown(destination.as_raw_fd(), libc::SHUT_WR) } < 0 {
                return Err(DirectionError {
                    operation: Operation::Write,
                    source: io::Error::last_os_error(),
                });
            }
            return Ok(());
        }
        while pipe.pending != 0 {
            let written = pipe
                .write_to(destination)
                .await
                .map_err(|source| DirectionError {
                    operation: Operation::Write,
                    source,
                })?;
            observer.transferred(written);
        }
        tokio::task::consume_budget().await;
    }
}

fn splice_once(read: RawFd, write: RawFd, bytes: usize) -> io::Result<usize> {
    loop {
        // SAFETY: callers keep socket/pipe owners alive. Both descriptors are
        // nonblocking; null offsets are required for these non-seekable fds.
        let count = unsafe {
            libc::splice(
                read,
                ptr::null_mut(),
                write,
                ptr::null_mut(),
                bytes,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
            )
        };
        if count >= 0 {
            return Ok(count as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
