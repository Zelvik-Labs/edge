//! Direct XSK readiness with owned registration and explicit empty-RX acknowledgement.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use tokio::io::Interest;
use tokio::io::unix::{AsyncFd, AsyncFdReadyGuard};

const XSK_INTEREST: Interest = Interest::READABLE.add(Interest::ERROR);

pub(crate) struct AfXdpReadiness {
    io: AsyncFd<OwnedFd>,
}

impl AfXdpReadiness {
    pub(crate) fn register(fd: BorrowedFd<'_>) -> io::Result<Self> {
        // The registration owns a duplicate, never a fabricated 'static borrow.
        // Ring operations and their zero-time poll remain nonblocking; this
        // does not change the shared open-file description's status flags.
        let socket = fd.try_clone_to_owned()?;
        Ok(Self {
            io: AsyncFd::with_interest(socket, XSK_INTEREST)?,
        })
    }

    pub(crate) async fn readable(&self) -> io::Result<AfXdpReadinessGuard<'_>> {
        // In Tokio 1.53 readable() masks out ERROR-only events. Request
        // ERROR explicitly in both registration and the awaited interest.
        let guard = self.io.ready(XSK_INTEREST).await?;
        let ready = guard.ready();
        // Tokio can return an Ok guard for EPOLLERR/HUP. These are faults,
        // just like POLLERR/HUP in the worker, not packet notifications.
        if ready.is_error() || ready.is_read_closed() || ready.is_write_closed() {
            return Err(io::Error::other(format!(
                "xsk readiness socket error event: {ready:?}"
            )));
        }
        Ok(AfXdpReadinessGuard { guard })
    }
}

pub(crate) struct AfXdpReadinessGuard<'a> {
    guard: AsyncFdReadyGuard<'a, OwnedFd>,
}

impl AfXdpReadinessGuard<'_> {
    /// Call only after an actual empty RX-ring observation made AFTER this
    /// guard was acquired. A successful or partial drain retains the guard.
    pub(crate) fn clear_after_empty(mut self) {
        self.guard.clear_ready();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn socket() -> UdpSocket {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
    }

    #[tokio::test]
    async fn idle_cancel_and_drop_release_owned_registration() {
        let socket = socket();
        let address = socket.local_addr().unwrap();
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
        let readiness = AfXdpReadiness::register(socket.as_fd()).unwrap();
        assert_eq!(
            unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) },
            flags
        );
        drop(socket);
        assert_eq!(
            UdpSocket::bind(address).unwrap_err().raw_os_error(),
            Some(libc::EADDRINUSE)
        );
        for _ in 0..3 {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), readiness.readable())
                    .await
                    .is_err()
            );
        }
        drop(readiness);
        UdpSocket::bind(address).expect("cancelled waits must not retain an XSK duplicate");
    }

    #[tokio::test]
    async fn partial_drain_and_yield_retain_readiness_until_empty() {
        let socket = socket();
        let readiness = AfXdpReadiness::register(socket.as_fd()).unwrap();
        let peer = self::socket();
        for sequence in 0u8..8 {
            peer.send_to(&[sequence], socket.local_addr().unwrap())
                .unwrap();
        }
        let mut guard = None;
        for sequence in 0u8..8 {
            guard = Some(
                tokio::time::timeout(Duration::from_secs(3), readiness.readable())
                    .await
                    .unwrap()
                    .unwrap(),
            );
            let mut frame = [0];
            assert_eq!(socket.recv(&mut frame).unwrap(), 1);
            assert_eq!(frame, [sequence]);
            tokio::task::yield_now().await;
        }
        assert_eq!(
            socket.recv(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        guard.unwrap().clear_after_empty();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), readiness.readable())
                .await
                .is_err()
        );
        peer.send_to(b"next", socket.local_addr().unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), readiness.readable())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn peer_close_is_a_fault_even_when_tokio_returns_an_ok_guard() {
        let (socket, peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let readiness = AfXdpReadiness::register(socket.as_fd()).unwrap();
        drop(peer);
        let result = tokio::time::timeout(Duration::from_secs(3), readiness.readable())
            .await
            .unwrap();
        assert!(result.is_err(), "HUP must reach the queue fault path");
    }

    #[tokio::test]
    async fn socket_error_is_a_fault_even_without_a_readable_packet() {
        let socket = socket();
        let peer = self::socket();
        socket.connect(peer.local_addr().unwrap()).unwrap();
        let readiness = AfXdpReadiness::register(socket.as_fd()).unwrap();
        drop(peer);
        socket.send(b"ICMP port unreachable").unwrap();
        let result = tokio::time::timeout(Duration::from_secs(3), readiness.readable())
            .await
            .unwrap();
        assert!(result.is_err(), "EPOLLERR must reach the queue fault path");
        assert_eq!(
            socket.take_error().unwrap().unwrap().kind(),
            io::ErrorKind::ConnectionRefused
        );
    }

    #[tokio::test]
    async fn registration_failure_preserves_original_descriptor() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let error = match AfXdpReadiness::register(file.as_fd()) {
            Ok(_) => panic!("epoll must reject a regular character device"),
            Err(error) => error,
        };
        assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        assert!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } >= 0);
    }
}
