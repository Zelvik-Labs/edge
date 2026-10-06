//! AF_XDP socket wake poller with an explicit, ownership-based cancel
//! path.
//!
//! The previous implementation spawned an unowned thread that captured a
//! raw socket fd and blocked in `poll(.., -1)`; when the reactor dropped,
//! nothing woke that poll, so the kernel file reference the poller held
//! kept the queue registration alive forever and later rebinds failed
//! `EBUSY`. `AfXdpWakeWorker` owns a duplicated socket fd and the write
//! end of a cancellation pipe; `shutdown` closes the hint channel, drops
//! the writer (an unconditional POLLHUP for the poller), and joins the
//! thread, so every exit path provably releases the socket.

use anyhow::Context;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use tokio::sync::mpsc;

/// Owned wake worker: the cancellation pipe writer, the OS thread, and
/// the hint channel receiver the queue bridge `select!`s on.
pub(crate) struct AfXdpWakeWorker {
    /// Write end of the cancel pipe. Dropping it makes the poller's read
    /// end report POLLHUP -- cancellation is closing, not a fallible write.
    stop: Option<OwnedFd>,
    join: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    rx: Option<mpsc::Receiver<()>>,
}

impl AfXdpWakeWorker {
    /// Duplicate `fd` for the poller and spawn the wake thread.
    ///
    /// `fd` is borrowed only for the `F_DUPFD_CLOEXEC` inside -- the
    /// duplicated `OwnedFd` the thread owns keeps the kernel queue
    /// registration alive until the worker shuts down, so dropping the
    /// caller's socket cannot free the registration behind the poller's
    /// back. That pin is released by `shutdown`/drop, never silently.
    pub(crate) fn start(fd: BorrowedFd<'_>, name: String) -> anyhow::Result<Self> {
        let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(io::Error::last_os_error()).context("xsk wake socket duplicate failed");
        }
        let socket = unsafe { OwnedFd::from_raw_fd(dup) };
        let (stop_read, stop_write) = cancel_pipe().context("xsk wake cancel pipe failed")?;
        let (tx, rx) = mpsc::channel::<()>(1);
        let join = std::thread::Builder::new()
            .name(name)
            .spawn(move || run_wake_loop(socket, stop_read, tx))
            .context("xsk wake thread spawn failed")?;
        Ok(Self {
            stop: Some(stop_write),
            join: Some(join),
            rx: Some(rx),
        })
    }

    /// Borrow the hint receiver slot -- the bridge select! arm polls it;
    /// shutdown closes it first so a `blocking_send`-blocked worker is
    /// released before the join.
    pub(crate) fn receiver_mut(&mut self) -> &mut Option<mpsc::Receiver<()>> {
        &mut self.rx
    }

    /// Whether the wake thread has exited (still joined here so its
    /// result is visible via `shutdown`).
    pub(crate) fn is_finished(&self) -> bool {
        self.join
            .as_ref()
            .map(|join| join.is_finished())
            .unwrap_or(true)
    }

    /// Release the worker in order: close the hint channel (unblocks a
    /// sender stuck in `blocking_send`), drop the cancel-pipe writer
    /// (guaranteed POLLHUP on the poller's read end), then join and
    /// surface the worker's result. A `None` join means the worker
    /// already exited and was joined -- teardown is then a no-op success.
    pub(crate) fn shutdown(&mut self) -> anyhow::Result<()> {
        if let Some(rx) = self.rx.as_mut() {
            rx.close();
        }
        drop(self.stop.take());
        if let Some(join) = self.join.take() {
            return match join.join() {
                Ok(result) => result,
                Err(_) => anyhow::bail!("xsk wake thread panicked"),
            };
        }
        Ok(())
    }
}

impl Drop for AfXdpWakeWorker {
    fn drop(&mut self) {
        // Wake pollers must never be abandoned: an unjoined thread keeps
        // its duplicated socket fd (and the kernel queue registration it
        // pins) alive past teardown. Failure is logged loudly, not
        // silently swallowed.
        if let Err(err) = self.shutdown() {
            tracing::error!("xsk wake worker shutdown failed: {err:#}");
        }
    }
}

/// O_CLOEXEC|O_NONBLOCK pipe: the worker polls the read end; the guard
/// owns only the write end. No bytes are ever written -- closing the
/// writer is the cancel signal itself.
fn cancel_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Cancel-pipe event classification. A faulted control descriptor is an
/// error, not a cancel; data on the pipe is unexpected input, also an
/// error; only the writer's close (POLLHUP) is a normal cancel request.
fn stop_requested(revents: i16) -> anyhow::Result<bool> {
    if revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        anyhow::bail!("xsk wake stop descriptor error event: {revents:#x}");
    }
    if revents & libc::POLLIN != 0 {
        anyhow::bail!("xsk wake unexpected stop descriptor data: {revents:#x}");
    }
    Ok(revents & libc::POLLHUP != 0)
}

/// Blocking poll over [stop-read, socket]; the stop event wins before
/// any packet hint is processed. `blocking_send` failure means the
/// receiver was closed intentionally: that is a clean teardown exit,
/// not an error.
fn run_wake_loop(socket: OwnedFd, stop_read: OwnedFd, tx: mpsc::Sender<()>) -> anyhow::Result<()> {
    let mut pfds = [
        libc::pollfd {
            fd: stop_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("xsk wake poll failed");
        }
        if rc == 0 {
            continue;
        }
        let stop_revents = pfds[0].revents;
        let socket_revents = pfds[1].revents;
        if stop_requested(stop_revents)? {
            return Ok(());
        }
        if socket_revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            anyhow::bail!("xsk wake socket error event: {socket_revents:#x}");
        }
        if socket_revents & libc::POLLIN != 0 && tx.blocking_send(()).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::AsFd;
    use std::time::{Duration, Instant};

    #[test]
    fn idle_worker_shutdown_joins_and_leaves_no_orphan() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut worker = AfXdpWakeWorker::start(socket.as_fd(), "wake-test-idle".into()).unwrap();
        let started = Instant::now();
        worker.shutdown().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "idle shutdown must join promptly"
        );
        assert!(worker.is_finished());
    }

    #[test]
    fn duplicated_fd_holds_registration_until_shutdown() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let mut worker = AfXdpWakeWorker::start(socket.as_fd(), "wake-test-dup".into()).unwrap();
        // The poller owns the duplicated fd -- closing the caller's
        // socket must NOT free the kernel registration.
        drop(socket);
        let err = UdpSocket::bind(addr).expect_err("rebind must fail while dup held");
        assert_eq!(err.raw_os_error(), Some(libc::EADDRINUSE));
        worker.shutdown().unwrap();
        UdpSocket::bind(addr).expect("bind must succeed after shutdown");
    }

    #[test]
    fn raii_drop_releases_socket_and_joins() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        {
            let worker = AfXdpWakeWorker::start(socket.as_fd(), "wake-test-raii".into()).unwrap();
            assert!(!worker.is_finished());
            drop(socket);
            assert_eq!(
                UdpSocket::bind(addr).unwrap_err().raw_os_error(),
                Some(libc::EADDRINUSE)
            );
        } // Drop -> shutdown -> cancel via writer close -> join
        UdpSocket::bind(addr).expect("bind must succeed after guard drop");
    }

    #[test]
    fn blocked_sender_released_by_shutdown() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut worker =
            AfXdpWakeWorker::start(socket.as_fd(), "wake-test-blocked".into()).unwrap();
        // One datagram: the hint lands on the capacity-1 channel.
        sender.send_to(b"a", addr).unwrap();
        let wait_full = |worker: &mut AfXdpWakeWorker| {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if worker
                    .receiver_mut()
                    .as_mut()
                    .map(|rx| rx.len() == 1)
                    .unwrap_or(false)
                {
                    return;
                }
                assert!(
                    Instant::now() <= deadline,
                    "hint channel must fill within 3s"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        wait_full(&mut worker);
        // A second datagram leaves the worker blocked in blocking_send
        // (channel stays full -- nobody drains it).
        sender.send_to(b"b", addr).unwrap();
        wait_full(&mut worker);
        assert!(
            !worker.is_finished(),
            "worker must still be alive while blocked on send"
        );
        let started = Instant::now();
        worker.shutdown().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "blocked send must be released by receiver close"
        );
    }

    #[test]
    fn stop_requested_classification() {
        assert_eq!(stop_requested(libc::POLLHUP).unwrap(), true);
        assert_eq!(stop_requested(0).unwrap(), false);
        for bad in [
            libc::POLLERR | libc::POLLHUP,
            libc::POLLNVAL | libc::POLLHUP,
            libc::POLLIN | libc::POLLHUP,
        ] {
            assert!(
                stop_requested(bad).is_err(),
                "revents {bad:#x} must be an error, not a cancel"
            );
        }
    }

    #[test]
    fn socket_hangup_is_an_observable_error() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(b);
        let mut worker = AfXdpWakeWorker::start(a.as_fd(), "wake-test-hup".into()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !worker.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(worker.is_finished(), "hangup must exit the worker");
        assert!(
            worker.shutdown().is_err(),
            "hangup must surface as Err, not a silent success"
        );
    }
}
