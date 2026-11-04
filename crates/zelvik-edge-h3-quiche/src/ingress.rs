//! Enforce the packet-size assumption behind the native connection reservation.
use std::io;
use tokio::net::UdpSocket;
use tokio_quiche::socket::QuicListener;

pub const MAX_DATAGRAM_BYTES: usize = 1500;

/// The upstream queue holds 2048 Vecs and its Linux recvmsg buffer is 64 KiB.
/// Disable GRO and reject larger wire datagrams in the kernel before that
/// allocation/queue boundary. GSO remains enabled for response transmission.
/// Returns the count of pre-profile datagrams drained before readiness.
pub fn bounded_listener(socket: UdpSocket) -> io::Result<(QuicListener, usize)> {
    #[cfg(target_os = "linux")]
    {
        linux::configure(socket)
    }
    #[cfg(not(target_os = "linux"))]
    {
        drop(socket);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded native QUIC ingress requires Linux socket filtering",
        ))
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::fd::AsRawFd;
    use tokio_quiche::socket::SocketCapabilitiesBuilder;

    pub(super) fn set<T>(socket: &UdpSocket, level: i32, option: i32, value: &T) -> io::Result<()> {
        // SAFETY: the live socket and initialized value remain valid for the
        // syscall; Linux copies the option (including the filter program).
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                option,
                std::ptr::from_ref(value).cast(),
                std::mem::size_of::<T>() as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn configure(socket: UdpSocket) -> io::Result<(QuicListener, usize)> {
        set(&socket, libc::SOL_UDP, libc::UDP_GRO, &0i32)?;
        // UDP socket filters see the 8-byte UDP header followed by payload,
        // for both IPv4 and IPv6. Accept whole packets or reject; never trim.
        let mut instructions = [
            libc::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_LEN) as u16,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: (MAX_DATAGRAM_BYTES + 8) as u32,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: 0,
            },
            libc::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: u32::MAX,
            },
        ];
        let program = libc::sock_fprog {
            len: instructions.len() as u16,
            filter: instructions.as_mut_ptr(),
        };
        set(&socket, libc::SOL_SOCKET, libc::SO_ATTACH_FILTER, &program)?;
        set(&socket, libc::SOL_SOCKET, libc::SO_LOCK_FILTER, &1i32)?;

        // Do not admit packets queued between bind and installing the filter.
        // Bound initialization work even if a peer sends continuously.
        let mut discarded = 0;
        let mut scratch = [0; 65536];
        loop {
            // Use the syscall directly: Tokio readiness may not yet have been
            // observed for a newly bound socket even if packets are queued.
            // SAFETY: scratch is writable for its entire length and the owned
            // socket stays live. MSG_DONTWAIT prevents blocking initialization.
            let received = unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    scratch.as_mut_ptr().cast(),
                    scratch.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            match received {
                value if value >= 0 => {
                    discarded += 1;
                    if discarded == 4096 {
                        return Err(io::Error::other(
                            "continuous traffic prevented bounded native QUIC ingress initialization",
                        ));
                    }
                }
                _ => {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    return Err(error);
                }
            }
        }
        let mut listener = QuicListener::try_from(socket)?;
        let mut capabilities = SocketCapabilitiesBuilder::new(&listener.socket);
        capabilities.gso()?;
        capabilities.check_udp_drop()?;
        if listener.socket.local_addr()?.is_ipv4() {
            capabilities.ipv4_pktinfo()?;
        } else {
            capabilities.ipv6_pktinfo()?;
        }
        listener.capabilities = capabilities.finish();
        Ok((listener, discarded))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn ipv4_ipv6_filter_rejects_oversize_without_trimming_or_breaking_gso() {
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                for address in ["127.0.0.1:0", "[::1]:0"] {
                    let (listener, discarded) = configure(UdpSocket::bind(address).await.unwrap()).unwrap();
                    assert_eq!(discarded, 0);
                    let sender = UdpSocket::bind(address).await.unwrap();
                    let peer = listener.socket.local_addr().unwrap();
                    for size in [1, 1200, 1350, 1500, 1501, 65000] {
                        sender.send_to(&vec![0x5a; size], peer).await.unwrap();
                    }
                    sender.send_to(b"end", peer).await.unwrap();
                    let mut buffer = vec![0; 65536];
                    let mut sizes = Vec::new();
                    loop {
                        let (size, _) = listener.socket.recv_from(&mut buffer).await.unwrap();
                        if &buffer[..size] == b"end" { break; }
                        assert!(buffer[..size].iter().all(|byte| *byte == 0x5a));
                        sizes.push(size);
                    }
                    assert_eq!(sizes, [1, 1200, 1350, 1500]);
                    set(&sender, libc::SOL_UDP, libc::UDP_SEGMENT, &1350i32).unwrap();
                    assert_eq!(sender.send_to(&vec![0x6b; 2700], peer).await.unwrap(), 2700);
                    for _ in 0..2 {
                        let (size, _) = listener.socket.recv_from(&mut buffer).await.unwrap();
                        assert_eq!(size, 1350);
                        assert!(buffer[..size].iter().all(|byte| *byte == 0x6b));
                    }
                    eprintln!("bounded-ingress {peer}: whole valid datagrams preserved, 1501/65000 rejected, GSO split into 2x1350");
                }
            }).await.expect("bounded ingress must preserve valid IPv4/IPv6 datagrams and GSO");
        }
    }
}
