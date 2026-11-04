async fn copy_stream_and_count<R, W>(
    server_id: i64,
    direction: StreamDirection,
    mut reader: R,
    mut writer: W,
    counter: &AtomicU64,
    options: RelayOptions,
) -> DirectionTaskResult
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let full_buf_bytes = relay_copy_buffer_bytes();
    let mut buf = vec![0u8; full_buf_bytes];
    let mut progress = RelayProgress::new(
        server_id,
        direction,
        counter,
        options.enforce_pressure_idle_timeout,
    );

    loop {
        // Regrow a pressure-shrunk buffer once data flows again; the shrink is
        // only an idle-time memory release, never a throughput cap.
        if buf.len() < full_buf_bytes {
            buf.resize(full_buf_bytes, 0);
        }
        let n = match read_with_pressure_idle_timeout(
            &mut reader,
            &mut buf,
            progress.pressure_idle_timeout,
        )
        .await
        {
            Ok(n) => n,
            Err(err) => {
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
        if n == 0 {
            progress.flush();
            writer
                .shutdown()
                .await
                .map_err(|err| DirectionTaskError::new(direction, RelayIoPhase::Write, err))?;
            return Ok(DirectionSuccess {
                close_reason: RelayCloseReason::Clean,
                close_detail: None,
            });
        }

        let mut remaining = &buf[..n];
        while !remaining.is_empty() {
            match writer.write(remaining).await {
                Ok(0) => {
                    return direction_error_or_benign(
                        direction,
                        RelayIoPhase::Write,
                        io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer"),
                    );
                }
                Ok(written) => {
                    // Count each successful write before another await. A
                    // later error or cancellation cannot erase accepted bytes.
                    progress.transferred(written);
                    remaining = &remaining[written..];
                }
                Err(err) => {
                    return direction_error_or_benign(direction, RelayIoPhase::Write, err);
                }
            }
        }
    }
}

fn direction_error_or_benign(
    direction: StreamDirection,
    phase: RelayIoPhase,
    err: io::Error,
) -> DirectionTaskResult {
    if is_benign_relay_io_error(&err) {
        let close_detail = RelayIoDetail::from_error(direction, phase, &err);
        Ok(DirectionSuccess {
            close_reason: RelayCloseReason::BenignIo,
            close_detail: Some(close_detail),
        })
    } else {
        Err(DirectionTaskError::new(direction, phase, err))
    }
}

fn is_benign_relay_io_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
    ) || is_benign_relay_raw_os_error(err)
}

fn is_benign_relay_raw_os_error(err: &io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(code)
            if code == libc::EPIPE
                || code == libc::ECONNRESET
                || code == libc::ENOTCONN
                || code == libc::ECONNABORTED
    )
}

struct RelayProgress<'a> {
    server_id: i64,
    direction: StreamDirection,
    counter: &'a AtomicU64,
    total: u64,
    unflushed: u64,
    pressure_idle_timeout: bool,
}

impl<'a> RelayProgress<'a> {
    fn new(
        server_id: i64,
        direction: StreamDirection,
        counter: &'a AtomicU64,
        pressure_idle_timeout: bool,
    ) -> Self {
        Self {
            server_id,
            direction,
            counter,
            total: 0,
            unflushed: 0,
            pressure_idle_timeout,
        }
    }
    fn flush(&mut self) {
        record_stream_metrics_delta(self.server_id, self.direction, self.unflushed);
        self.unflushed = 0;
    }
}

impl RelayProgress<'_> {
    fn transferred(&mut self, bytes: usize) {
        self.total += bytes as u64;
        self.unflushed += bytes as u64;
        self.counter.store(self.total, Ordering::Relaxed);
        if self.unflushed >= STREAM_METRICS_FLUSH_BYTES {
            self.flush();
        }
    }
}

impl Drop for RelayProgress<'_> {
    fn drop(&mut self) {
        self.flush();
    }
}

