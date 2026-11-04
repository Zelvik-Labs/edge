// Zero-sized error payloads keep the existing Display and Debug text.
// io::Error still owns its custom error representation; no zero-allocation
// claim is made about that outer representation.
struct UdpDownstreamChannelError<const CLOSED: bool>;

impl<const CLOSED: bool> UdpDownstreamChannelError<CLOSED> {
    fn message(&self) -> &'static str {
        if CLOSED {
            "UDP downstream channel is closed"
        } else {
            "UDP downstream channel is full"
        }
    }
}

impl<const CLOSED: bool> std::fmt::Display for UdpDownstreamChannelError<CLOSED> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl<const CLOSED: bool> std::fmt::Debug for UdpDownstreamChannelError<CLOSED> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.message(), formatter)
    }
}

impl<const CLOSED: bool> std::error::Error for UdpDownstreamChannelError<CLOSED> {}

#[derive(Debug)]
pub struct ChannelUdpDownstreamSender {
    listen_addr: SocketAddr,
    tx: mpsc::Sender<DownstreamUdpDatagram>,
}

impl ChannelUdpDownstreamSender {
    fn try_send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.try_send_owned_to(Bytes::copy_from_slice(data), target)
    }

    fn try_send_owned_to(&self, data: Bytes, target: SocketAddr) -> io::Result<usize> {
        let len = data.len();
        match self.tx.try_send(DownstreamUdpDatagram {
            listen_addr: self.listen_addr,
            peer_addr: target,
            payload: data,
            #[cfg(target_os = "linux")]
            enqueued_ms: udp_activity_now_ms(),
        }) {
            Ok(()) => Ok(len),
            Err(mpsc::error::TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                UdpDownstreamChannelError::<false>,
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                UdpDownstreamChannelError::<true>,
            )),
        }
    }
}
