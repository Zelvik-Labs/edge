//! Build target for the independent Linux AF_XDP wake benchmark.

pub mod ffi;

#[cfg(test)]
#[path = "../../../../src/xdp/af_xdp/wake.rs"]
mod legacy;

#[cfg(test)]
mod readiness_tests {
    use std::io;
    use std::net::UdpSocket;
    use std::time::Duration;
    use tokio::io::unix::AsyncFd;

    #[tokio::test]
    async fn writable_interest_alone_does_not_resolve_readable() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let io = AsyncFd::new(socket).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), io.readable())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn partial_burst_preserves_readiness_until_would_block() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let address = socket.local_addr().unwrap();
        let io = AsyncFd::new(socket).unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        for sequence in 0u8..8 {
            peer.send_to(&[sequence], address).unwrap();
        }
        for sequence in 0u8..8 {
            let mut guard = tokio::time::timeout(Duration::from_secs(5), io.readable())
                .await
                .unwrap()
                .unwrap();
            let mut frame = [0; 1];
            assert_eq!(io.get_ref().recv(&mut frame).unwrap(), 1);
            assert_eq!(frame, [sequence]);
            if sequence == 7 {
                assert_eq!(
                    io.get_ref().recv(&mut frame).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                guard.clear_ready();
            }
            tokio::task::yield_now().await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), io.readable())
                .await
                .is_err()
        );
    }
}
