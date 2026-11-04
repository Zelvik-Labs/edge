#[derive(Debug)]
pub struct ChannelUdpDownstreamSender {
    listen_addr: SocketAddr,
    tx: mpsc::Sender<DownstreamUdpDatagram>,
}

impl ChannelUdpDownstreamSender {
    fn try_send_to(&self, data: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.try_send_with(target, || Bytes::copy_from_slice(data))
    }

    fn try_send_owned_to(&self, data: Bytes, target: SocketAddr) -> io::Result<usize> {
        self.try_send_with(target, || data)
    }

    fn try_send_with(
        &self,
        target: SocketAddr,
        payload: impl FnOnce() -> Bytes,
    ) -> io::Result<usize> {
        match self.tx.try_reserve() {
            Ok(permit) => {
                let data = payload();
                let len = data.len();
                permit.send(DownstreamUdpDatagram {
                    listen_addr: self.listen_addr,
                    peer_addr: target,
                    payload: data,
                    #[cfg(target_os = "linux")]
                    enqueued_ms: udp_activity_now_ms(),
                });
                Ok(len)
            }
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
