async fn copy_stream_and_count<R, W>(
    server_id: i64,
    direction: StreamDirection,
    mut reader: R,
    mut writer: W,
    counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    options: RelayOptions,
) -> DirectionTaskResult
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let full_buf_bytes = relay_copy_buffer_bytes();
    let mut buf = vec![0u8; full_buf_bytes];
    let mut total = 0u64;
    let mut unflushed = 0u64;

    loop {
        // Regrow a pressure-shrunk buffer once data flows again; the shrink is
        // only an idle-time memory release, never a throughput cap.
        if buf.len() < full_buf_bytes {
            buf.resize(full_buf_bytes, 0);
        }
        let n = match read_with_pressure_idle_timeout(
            &mut reader,
            &mut buf,
            options.enforce_pressure_idle_timeout,
        )
        .await
        {
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
        if n == 0 {
            record_stream_metrics_delta(server_id, direction, unflushed);
            writer
                .shutdown()
                .await
                .map_err(|err| DirectionTaskError::new(direction, RelayIoPhase::Write, err))?;
            return Ok(DirectionSuccess {
                close_reason: RelayCloseReason::Clean,
                close_detail: None,
            });
        }

        if let Err(err) = writer.write_all(&buf[..n]).await {
            record_stream_metrics_delta(server_id, direction, unflushed);
            return direction_error_or_benign(direction, RelayIoPhase::Write, err);
        }
        total += n as u64;
        unflushed += n as u64;
        counter.store(total, std::sync::atomic::Ordering::Relaxed);
        if unflushed >= STREAM_METRICS_FLUSH_BYTES {
            record_stream_metrics_delta(server_id, direction, unflushed);
            unflushed = 0;
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
