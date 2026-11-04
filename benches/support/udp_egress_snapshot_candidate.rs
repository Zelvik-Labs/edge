#[derive(Debug)]
pub struct ChannelUdpDownstreamSender {
    listen_addr: SocketAddr,
    tx: mpsc::Sender<DownstreamUdpDatagram>,
}

impl ChannelUdpDownstreamSender {
    fn try_send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        // Advisory snapshots reject known-unavailable queues before copying.
        // A competing producer may still fill the queue after this check;
        // try_send remains the authoritative admission operation below.
        if self.tx.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "UDP downstream channel is closed",
            ));
        }
        if self.tx.capacity() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "UDP downstream channel is full",
            ));
        }
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
                "UDP downstream channel is full",
            )),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "UDP downstream channel is closed",
            )),
        }
    }
}
