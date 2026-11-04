//! Real kernel controls for the optional shared QUIC TX component candidate.
include!("candidate.rs");

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::net::UdpSocket;

fn bind(ip: IpAddr) -> (UdpSocket, KernelQuicTx) {
    let domain = if ip.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket =
        socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)).unwrap();
    if ip.is_ipv6() {
        socket.set_only_v6(true).unwrap();
    }
    let tx = KernelQuicTx::configure_before_bind(&socket).unwrap();
    assert!(!udp_gro_enabled(&socket).unwrap());
    socket.bind(&SocketAddr::new(ip, 0).into()).unwrap();
    let socket = UdpSocket::from_std(socket.into()).unwrap();
    assert!(!udp_gro_enabled(&socket).unwrap());
    (socket, tx)
}

async fn control(ip: IpAddr) {
    let (sender, tx) = bind(ip);
    let receiver = UdpSocket::bind(SocketAddr::new(ip, 0)).await.unwrap();
    let destination = receiver.local_addr().unwrap();
    assert!(
        tx.max_segments() >= 8,
        "kernel did not expose the candidate GSO capability"
    );
    let payload: Vec<_> = (1..=8u8).flat_map(|index| vec![index; 1200]).collect();
    let transmit = quinn_udp::Transmit {
        destination,
        ecn: Some(quinn_udp::EcnCodepoint::Ect0),
        contents: &payload,
        segment_size: Some(1200),
        src_ip: Some(ip),
    };
    sender.writable().await.unwrap();
    tx.try_send(&sender, &transmit).unwrap();
    let mut buffer = vec![0u8; 65536];
    for expected in 1..=8u8 {
        let (length, source) =
            tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(length, 1200);
        assert_eq!(source, sender.local_addr().unwrap());
        assert!(buffer[..length].iter().all(|byte| *byte == expected));
    }
    assert!(!udp_gro_enabled(&sender).unwrap());

    // Clear Tokio's write readiness with the same WouldBlock contract used
    // by a real full egress socket. The candidate must honor it, not bypass
    // the runtime registration and send a prefix of the next batch.
    let cleared: io::Result<()> = sender.try_io(tokio::io::Interest::WRITABLE, || {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    });
    assert_eq!(cleared.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    assert_eq!(
        tx.try_send(&sender, &transmit).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(
        receiver.try_recv_from(&mut buffer).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    sender.writable().await.unwrap();
    tx.try_send(&sender, &transmit).unwrap();
    for _ in 0..8 {
        let (length, _) =
            tokio::time::timeout(Duration::from_secs(2), receiver.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(length, 1200);
    }

    let oversized = vec![7; 65536];
    let transmit = quinn_udp::Transmit {
        destination,
        ecn: None,
        contents: &oversized,
        segment_size: None,
        src_ip: Some(ip),
    };
    sender.writable().await.unwrap();
    let error = tx.try_send(&sender, &transmit).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::EMSGSIZE));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), receiver.recv_from(&mut buffer))
            .await
            .is_err()
    );
    println!(
        "family={} segments={} gro=false fragmentation={} delivered=16x1200 readiness=PASS oversize_errno={}",
        if ip.is_ipv4() { "IPv4" } else { "IPv6" },
        tx.max_segments(),
        tx.may_fragment(),
        libc::EMSGSIZE
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Initializing an already-live socket must fail before changing options.
    let bound = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .unwrap();
    bound
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    let before = udp_gro_enabled(&bound).unwrap();
    assert!(
        matches!(KernelQuicTx::configure_before_bind(&bound), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
    );
    assert_eq!(udp_gro_enabled(&bound).unwrap(), before);
    control(IpAddr::V4(Ipv4Addr::LOCALHOST)).await;
    control(IpAddr::V6(Ipv6Addr::LOCALHOST)).await;
    println!("PASS: candidate only; no production integration or throughput claim");
}
