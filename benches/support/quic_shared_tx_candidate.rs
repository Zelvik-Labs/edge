// Linux component candidate only; not installed in the production shared path.
use std::io;
use std::os::fd::{AsFd, AsRawFd};

fn udp_gro_enabled(socket: &impl AsFd) -> io::Result<bool> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&value) as libc::socklen_t;
    // SAFETY: value/size are initialized writable objects of the declared size;
    // the borrowed descriptor remains valid throughout this synchronous call.
    let result = unsafe {
        libc::getsockopt(
            socket.as_fd().as_raw_fd(),
            libc::SOL_UDP,
            libc::UDP_GRO,
            (&mut value as *mut libc::c_int).cast(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if size as usize != std::mem::size_of_val(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected UDP_GRO option size",
        ));
    }
    Ok(value != 0)
}

struct KernelQuicTx {
    state: quinn_udp::UdpSocketState,
}

impl KernelQuicTx {
    fn configure_before_bind(socket: &socket2::Socket) -> io::Result<Self> {
        if !socket
            .local_addr()?
            .as_socket()
            .is_some_and(|address| address.port() == 0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "QUIC TX setup requires an unbound socket",
            ));
        }
        // This API also enables receive GRO. Disable it before the socket has
        // a bound address, so no coalesced datagram can reach the plain demux.
        let state = quinn_udp::UdpSocketState::new(socket.into())?;
        let disabled: libc::c_int = 0;
        // SAFETY: a live borrowed descriptor and a readable integer option;
        // setsockopt does not retain the pointer after returning.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_UDP,
                libc::UDP_GRO,
                (&disabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&disabled) as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        if udp_gro_enabled(socket)? {
            return Err(io::Error::other("UDP_GRO remained enabled before bind"));
        }
        Ok(Self { state })
    }

    fn try_send(
        &self,
        socket: &tokio::net::UdpSocket,
        transmit: &quinn_udp::Transmit<'_>,
    ) -> io::Result<()> {
        // Native Linux sendmsg accepts the complete segmented transmit or
        // returns an error. Keep errors visible; UdpSocketState::send would
        // convert several non-WouldBlock failures to success.
        socket.try_io(tokio::io::Interest::WRITABLE, || {
            self.state.try_send(socket.into(), transmit)
        })
    }

    fn max_segments(&self) -> usize {
        self.state.max_gso_segments()
    }
    fn may_fragment(&self) -> bool {
        self.state.may_fragment()
    }
}
