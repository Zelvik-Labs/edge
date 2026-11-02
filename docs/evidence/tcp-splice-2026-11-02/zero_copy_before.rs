
    use super::*;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    const SPLICE_CHUNK_BYTES: usize = 256 * 1024;
    const PIPE_SIZE_BYTES: libc::c_int = 256 * 1024;
    const POLL_TIMEOUT_MS: libc::c_int = 1000;

    pub(super) enum ZeroCopyOutcome {
        Completed(Result<RelayOutcome, BidirectionalStreamError>),
        Fallback,
    }

    pub(super) async fn stream_bidirectional(
        server_id: i64,
        client: &TcpStream,
        backend: &TcpStream,
        options: RelayOptions,
    ) -> ZeroCopyOutcome {
        let client_fd = client.as_raw_fd();
        let backend_fd = backend.as_raw_fd();
        let client_to_backend = match DirectionFds::new(client_fd, backend_fd) {
            Ok(fds) => fds,
            Err(_) => return ZeroCopyOutcome::Fallback,
        };
        let backend_to_client = match DirectionFds::new(backend_fd, client_fd) {
            Ok(fds) => fds,
            Err(_) => return ZeroCopyOutcome::Fallback,
        };
        let shutdown_fds = match ShutdownFds::new(client_fd, backend_fd) {
            Ok(fds) => Arc::new(fds),
            Err(_) => return ZeroCopyOutcome::Fallback,
        };

        let bytes_received_counter = Arc::new(AtomicU64::new(0));
        let bytes_sent_counter = Arc::new(AtomicU64::new(0));
        let control = Arc::new(RelayControl::default());

        let cancel_rx = options.cancel_rx.clone();
        let mut c2b = spawn_direction(
            server_id,
            StreamDirection::ClientToBackend,
            client_to_backend,
            bytes_received_counter.clone(),
            control.clone(),
            options.clone(),
        );
        let mut b2c = spawn_direction(
            server_id,
            StreamDirection::BackendToClient,
            backend_to_client,
            bytes_sent_counter.clone(),
            control.clone(),
            options,
        );

        let (first_direction, first) = tokio::select! {
            result = &mut c2b => (
                StreamDirection::ClientToBackend,
                flatten_join_result(StreamDirection::ClientToBackend, result),
            ),
            result = &mut b2c => (
                StreamDirection::BackendToClient,
                flatten_join_result(StreamDirection::BackendToClient, result),
            ),
            cancel_reason = wait_for_connection_cancel(cancel_rx) => {
                control.stop();
                shutdown_fds.shutdown_all();
                (
                    StreamDirection::ClientToBackend,
                    Ok(DirectionSuccess {
                        close_reason: relay_close_reason_for_cancel(cancel_reason),
                        close_detail: None,
                    }),
                )
            },
        };

        let result = if first_direction == StreamDirection::ClientToBackend {
            finish_zero_copy_after_first(
                first,
                StreamDirection::BackendToClient,
                b2c,
                &control,
                &shutdown_fds,
                &bytes_received_counter,
                &bytes_sent_counter,
            )
            .await
        } else {
            finish_zero_copy_after_first(
                first,
                StreamDirection::ClientToBackend,
                c2b,
                &control,
                &shutdown_fds,
                &bytes_received_counter,
                &bytes_sent_counter,
            )
            .await
        };
        ZeroCopyOutcome::Completed(result)
    }

    fn spawn_direction(
        server_id: i64,
        direction: StreamDirection,
        fds: DirectionFds,
        counter: Arc<AtomicU64>,
        control: Arc<RelayControl>,
        options: RelayOptions,
    ) -> tokio::task::JoinHandle<DirectionTaskResult> {
        tokio::task::spawn_blocking(move || {
            splice_direction_and_count(server_id, direction, fds, counter, &control, options)
        })
    }

    fn flatten_join_result(
        direction: StreamDirection,
        result: Result<DirectionTaskResult, tokio::task::JoinError>,
    ) -> DirectionTaskResult {
        result.unwrap_or_else(|err| {
            Err(DirectionTaskError::new(
                direction,
                RelayIoPhase::Read,
                io::Error::other(err.to_string()),
            ))
        })
    }

    async fn finish_zero_copy_after_first(
        first: DirectionTaskResult,
        _remaining_direction: StreamDirection,
        mut remaining: tokio::task::JoinHandle<DirectionTaskResult>,
        control: &RelayControl,
        shutdown_fds: &ShutdownFds,
        bytes_received_counter: &AtomicU64,
        bytes_sent_counter: &AtomicU64,
    ) -> Result<RelayOutcome, BidirectionalStreamError> {
        control.stop();
        shutdown_fds.shutdown_all();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(POLL_TIMEOUT_MS as u64 + 500),
            &mut remaining,
        )
        .await;
        finish_relay_after_first(first, bytes_received_counter, bytes_sent_counter)
    }

    fn splice_direction_and_count(
        server_id: i64,
        direction: StreamDirection,
        fds: DirectionFds,
        counter: Arc<AtomicU64>,
        control: &RelayControl,
        options: RelayOptions,
    ) -> DirectionTaskResult {
        let mut total = 0u64;
        let mut unflushed = 0u64;
        let mut idle = std::time::Duration::ZERO;

        loop {
            if control.should_stop() {
                record_stream_metrics_delta(server_id, direction, unflushed);
                return Ok(DirectionSuccess {
                    close_reason: RelayCloseReason::BenignIo,
                    close_detail: None,
                });
            }

            let pressure_timeout = options
                .enforce_pressure_idle_timeout
                .then(|| MEMORY_GOVERNOR.tcp_relay_pressure_idle_timeout())
                .flatten();

            let moved_to_pipe = match splice_socket_to_pipe(
                fds.read.fd(),
                fds.pipe_write.fd(),
                control,
                &mut idle,
                pressure_timeout,
            ) {
                Ok(n) => n,
                Err(err) => {
                    record_stream_metrics_delta(server_id, direction, unflushed);
                    if err.kind() == io::ErrorKind::TimedOut {
                        return Ok(DirectionSuccess {
                            close_reason: RelayCloseReason::PressureIdleTimeout,
                            close_detail: Some(RelayIoDetail {
                                direction,
                                phase: RelayIoPhase::Read,
                                raw_os_error: None,
                            }),
                        });
                    }
                    return direction_error_or_benign(direction, RelayIoPhase::Read, err);
                }
            };
            if moved_to_pipe == 0 {
                record_stream_metrics_delta(server_id, direction, unflushed);
                return Ok(DirectionSuccess {
                    close_reason: RelayCloseReason::Clean,
                    close_detail: None,
                });
            }
            idle = std::time::Duration::ZERO;

            let mut remaining = moved_to_pipe;
            while remaining > 0 {
                let written = match splice_pipe_to_socket(
                    fds.pipe_read.fd(),
                    fds.write.fd(),
                    remaining,
                    control,
                ) {
                    Ok(n) => n,
                    Err(err) => {
                        record_stream_metrics_delta(server_id, direction, unflushed);
                        return direction_error_or_benign(direction, RelayIoPhase::Write, err);
                    }
                };
                if written == 0 {
                    record_stream_metrics_delta(server_id, direction, unflushed);
                    return Err(DirectionTaskError::new(
                        direction,
                        RelayIoPhase::Write,
                        io::Error::new(
                            io::ErrorKind::WriteZero,
                            "zero-copy splice wrote zero bytes",
                        ),
                    ));
                }
                remaining -= written;
                total += written as u64;
                unflushed += written as u64;
                counter.store(total, Ordering::Relaxed);
                if unflushed >= STREAM_METRICS_FLUSH_BYTES {
                    record_stream_metrics_delta(server_id, direction, unflushed);
                    unflushed = 0;
                }
            }
        }
    }

    fn splice_socket_to_pipe(
        read_fd: RawFd,
        pipe_write_fd: RawFd,
        control: &RelayControl,
        idle: &mut std::time::Duration,
        pressure_timeout: Option<std::time::Duration>,
    ) -> io::Result<usize> {
        loop {
            match splice_once(read_fd, pipe_write_fd, SPLICE_CHUNK_BYTES) {
                Err(err) if is_would_block(&err) => {
                    poll_fd(read_fd, libc::POLLIN, control, Some(idle), pressure_timeout)?
                }
                other => return other,
            }
        }
    }

    fn splice_pipe_to_socket(
        pipe_read_fd: RawFd,
        write_fd: RawFd,
        len: usize,
        control: &RelayControl,
    ) -> io::Result<usize> {
        loop {
            match splice_once(pipe_read_fd, write_fd, len) {
                Err(err) if is_would_block(&err) => {
                    poll_fd(write_fd, libc::POLLOUT, control, None, None)?
                }
                other => return other,
            }
        }
    }

    fn splice_once(read_fd: RawFd, write_fd: RawFd, len: usize) -> io::Result<usize> {
        loop {
            let n = unsafe {
                libc::splice(
                    read_fd,
                    ptr::null_mut(),
                    write_fd,
                    ptr::null_mut(),
                    len,
                    (libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK) as libc::c_uint,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
    }

    fn poll_fd(
        fd: RawFd,
        events: libc::c_short,
        control: &RelayControl,
        mut idle: Option<&mut std::time::Duration>,
        pressure_timeout: Option<std::time::Duration>,
    ) -> io::Result<()> {
        loop {
            if control.should_stop() {
                return Err(stop_error());
            }
            let poll_timeout_ms = pressure_timeout
                .map(|timeout| {
                    timeout.min(std::time::Duration::from_secs(1)).as_millis() as libc::c_int
                })
                .unwrap_or(POLL_TIMEOUT_MS);
            let mut poll_fd = libc::pollfd {
                fd,
                events,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut poll_fd, 1, poll_timeout_ms) };
            if result > 0 {
                return Ok(());
            }
            if result == 0 {
                if let Some(idle) = idle.as_deref_mut() {
                    *idle += std::time::Duration::from_millis(poll_timeout_ms.max(1) as u64);
                    if let Some(timeout) = pressure_timeout
                        && *idle >= timeout
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("zero-copy TCP relay idle under pressure for {:?}", timeout),
                        ));
                    }
                }
                continue;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
    }

    fn is_would_block(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK)
    }

    fn stop_error() -> io::Error {
        io::Error::new(io::ErrorKind::Interrupted, "zero-copy relay stopped")
    }

    #[derive(Default)]
    struct RelayControl {
        stop: AtomicBool,
    }

    impl RelayControl {
        fn stop(&self) {
            self.stop.store(true, Ordering::Relaxed);
        }

        fn should_stop(&self) -> bool {
            self.stop.load(Ordering::Relaxed)
        }
    }

    struct DirectionFds {
        read: OwnedFd,
        write: OwnedFd,
        pipe_read: OwnedFd,
        pipe_write: OwnedFd,
    }

    impl DirectionFds {
        fn new(read_fd: RawFd, write_fd: RawFd) -> io::Result<Self> {
            let read = dup_fd(read_fd)?;
            let write = dup_fd(write_fd)?;
            let (pipe_read, pipe_write) = create_pipe()?;
            let _ = unsafe { libc::fcntl(pipe_read.fd(), libc::F_SETPIPE_SZ, PIPE_SIZE_BYTES) };
            Ok(Self {
                read,
                write,
                pipe_read,
                pipe_write,
            })
        }
    }

    struct ShutdownFds {
        client: OwnedFd,
        backend: OwnedFd,
    }

    impl ShutdownFds {
        fn new(client_fd: RawFd, backend_fd: RawFd) -> io::Result<Self> {
            Ok(Self {
                client: dup_fd(client_fd)?,
                backend: dup_fd(backend_fd)?,
            })
        }

        fn shutdown_all(&self) {
            unsafe {
                libc::shutdown(self.client.fd(), libc::SHUT_RDWR);
                libc::shutdown(self.backend.fd(), libc::SHUT_RDWR);
            }
        }
    }

    struct OwnedFd(RawFd);

    impl OwnedFd {
        fn fd(&self) -> RawFd {
            self.0
        }
    }

    impl Drop for OwnedFd {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.0);
            }
        }
    }

    fn dup_fd(fd: RawFd) -> io::Result<OwnedFd> {
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(OwnedFd(duplicated))
        }
    }

    fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0; 2];
        let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((OwnedFd(fds[0]), OwnedFd(fds[1])))
        }
    }