//! Exact copy-loop component probe. Policy lookup and metric-map lookup are
//! replaced by fixed fixtures; this does not measure whole-node TCP throughput.
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

include!("allocator.rs");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamDirection {
    ClientToBackend,
    BackendToClient,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayIoPhase {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayCloseReason {
    Clean,
    BenignIo,
    PressureIdleTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RelayIoDetail {
    direction: StreamDirection,
    phase: RelayIoPhase,
    raw_os_error: Option<i32>,
}

impl RelayIoDetail {
    fn from_error(direction: StreamDirection, phase: RelayIoPhase, error: &io::Error) -> Self {
        Self {
            direction,
            phase,
            raw_os_error: error.raw_os_error(),
        }
    }
}

#[derive(Debug)]
struct DirectionSuccess {
    close_reason: RelayCloseReason,
    close_detail: Option<RelayIoDetail>,
}

#[derive(Debug)]
struct DirectionTaskError {
    direction: StreamDirection,
    phase: RelayIoPhase,
    source: io::Error,
}

impl DirectionTaskError {
    fn new(direction: StreamDirection, phase: RelayIoPhase, source: io::Error) -> Self {
        Self {
            direction,
            phase,
            source,
        }
    }
}

type DirectionTaskResult = Result<DirectionSuccess, DirectionTaskError>;

// Only this field is read by the production direction function. Pressure is
// disabled in every fixture, matching long-lived SNI passthrough policy.
struct RelayOptions {
    enforce_pressure_idle_timeout: bool,
}

const STREAM_METRICS_FLUSH_BYTES: u64 = 1024 * 1024;
static FLUSHED: AtomicU64 = AtomicU64::new(0);

fn relay_copy_buffer_bytes() -> usize {
    64 * 1024
}

async fn read_with_pressure_idle_timeout<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    enforce_pressure_idle_timeout: bool,
) -> io::Result<usize> {
    assert!(!enforce_pressure_idle_timeout);
    reader.read(buf.as_mut_slice()).await
}

fn record_stream_metrics_delta(_id: i64, _direction: StreamDirection, bytes: u64) {
    if bytes != 0 {
        FLUSHED.fetch_add(bytes, Ordering::Relaxed);
    }
}

include!("production.rs");
include!("counter.rs");

struct CopyWriter<'a> {
    output: &'a mut [u8],
    position: usize,
    chunk: usize,
}

impl AsyncWrite for CopyWriter<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let n = buf
            .len()
            .min(self.chunk)
            .min(self.output.len() - self.position);
        let start = self.position;
        self.output[start..start + n].copy_from_slice(&buf[..n]);
        self.position += n;
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn complete(input: &[u8], output: &mut [u8], chunk: usize) {
    let counter = new_counter();
    let mut writer = CopyWriter {
        output,
        position: 0,
        chunk,
    };
    let result = {
        let transfer = copy_stream_and_count(
            1,
            StreamDirection::ClientToBackend,
            black_box(input),
            &mut writer,
            counter_arg(&counter),
            RelayOptions {
                enforce_pressure_idle_timeout: false,
            },
        );
        let mut transfer = std::pin::pin!(transfer);
        let mut context = Context::from_waker(Waker::noop());
        match std::future::Future::poll(transfer.as_mut(), &mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("ready in-memory fixture unexpectedly suspended"),
        }
    }
    .unwrap();
    assert_eq!(result.close_reason, RelayCloseReason::Clean);
    assert!(result.close_detail.is_none());
    assert_eq!(counter.load(Ordering::Relaxed), input.len() as u64);
    assert_eq!(writer.position, input.len());
    black_box(writer);
}

#[derive(Clone, Copy)]
enum AfterWrite {
    Pending,
    Error,
    Zero,
}

struct PartialWriter {
    remaining: usize,
    after: AfterWrite,
}
impl AsyncWrite for PartialWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.remaining > 0 {
            let n = self.remaining.min(buf.len());
            self.remaining -= n;
            return Poll::Ready(Ok(n));
        }
        match self.after {
            AfterWrite::Pending => Poll::Pending,
            AfterWrite::Error => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            AfterWrite::Zero => Poll::Ready(Ok(0)),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct OneThenPending {
    sent: bool,
}
impl AsyncRead for OneThenPending {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.sent {
            return Poll::Pending;
        }
        self.sent = true;
        buf.put_slice(b"abcdef");
        Poll::Ready(Ok(()))
    }
}

fn controls(require_exact: bool) {
    println!("case\taccepted\tcounter\tflushed");
    for (name, after) in [
        ("partial_error", AfterWrite::Error),
        ("partial_zero", AfterWrite::Zero),
        ("partial_cancel", AfterWrite::Pending),
    ] {
        FLUSHED.store(0, Ordering::Relaxed);
        let counter = new_counter();
        {
            let transfer = copy_stream_and_count(
                1,
                StreamDirection::BackendToClient,
                &b"abcdef"[..],
                PartialWriter {
                    remaining: 3,
                    after,
                },
                counter_arg(&counter),
                RelayOptions {
                    enforce_pressure_idle_timeout: false,
                },
            );
            let mut transfer = std::pin::pin!(transfer);
            let mut cx = Context::from_waker(Waker::noop());
            match (after, std::future::Future::poll(transfer.as_mut(), &mut cx)) {
                (AfterWrite::Pending, Poll::Pending) => {}
                (AfterWrite::Error, Poll::Ready(Ok(success))) => {
                    assert_eq!(success.close_reason, RelayCloseReason::BenignIo);
                    let detail = success.close_detail.unwrap();
                    assert_eq!(
                        (detail.direction, detail.phase),
                        (StreamDirection::BackendToClient, RelayIoPhase::Write)
                    );
                }
                (AfterWrite::Zero, Poll::Ready(Err(error))) => {
                    assert_eq!(error.source.kind(), io::ErrorKind::WriteZero);
                    assert_eq!(
                        (error.direction, error.phase),
                        (StreamDirection::BackendToClient, RelayIoPhase::Write)
                    );
                }
                _ => panic!("unexpected control outcome"),
            }
        }
        let values = (
            counter.load(Ordering::Relaxed),
            FLUSHED.load(Ordering::Relaxed),
        );
        println!("{name}\t3\t{}\t{}", values.0, values.1);
        if require_exact {
            assert_eq!(
                values,
                (3, 3),
                "accepted partial writes must survive error or cancellation"
            );
        }
    }
    FLUSHED.store(0, Ordering::Relaxed);
    let counter = new_counter();
    {
        let transfer = copy_stream_and_count(
            1,
            StreamDirection::ClientToBackend,
            OneThenPending { sent: false },
            tokio::io::sink(),
            counter_arg(&counter),
            RelayOptions {
                enforce_pressure_idle_timeout: false,
            },
        );
        let mut transfer = std::pin::pin!(transfer);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(std::future::Future::poll(transfer.as_mut(), &mut cx).is_pending());
    }
    let values = (
        counter.load(Ordering::Relaxed),
        FLUSHED.load(Ordering::Relaxed),
    );
    println!("read_cancel\t6\t{}\t{}", values.0, values.1);
    if require_exact {
        assert_eq!(values, (6, 6));
    }
}

fn main() {
    let mode = std::env::args().nth(1).expect("choose a probe mode");
    if mode == "--controls" || mode == "--assert-accounting" {
        controls(mode == "--assert-accounting");
        return;
    }
    if mode == "--timing" {
        println!("case\tblock\titerations\tnanoseconds");
    }
    #[cfg(feature = "count-allocations")]
    if mode == "--allocations" {
        println!("case\tallocations\treallocations\trequested_bytes\tmax_allocation_bytes");
    }
    for (name, size, chunk) in [
        ("1KiB_full", 1024, usize::MAX),
        ("64KiB_full", 65536, usize::MAX),
        ("1MiB_full", 1048576, usize::MAX),
        ("64KiB_short64", 65536, 64),
    ] {
        let input = vec![0x5a; size];
        let mut output = vec![0; size];
        FLUSHED.store(0, Ordering::Relaxed);
        complete(&input, &mut output, chunk);
        assert_eq!(input, output);
        assert_eq!(FLUSHED.load(Ordering::Relaxed), size as u64);
        match mode.as_str() {
            "--timing" => {
                let start = Instant::now();
                for _ in 0..32 {
                    complete(&input, &mut output, chunk);
                }
                let per_call = (start.elapsed().as_nanos() / 32).max(1);
                let iterations = (10_000_000 / per_call).clamp(16, 65536) as usize;
                for block in 0..20 {
                    let start = Instant::now();
                    for _ in 0..iterations {
                        complete(&input, &mut output, chunk);
                    }
                    println!(
                        "{name}\t{block}\t{iterations}\t{}",
                        start.elapsed().as_nanos()
                    );
                }
                assert_eq!(input, output);
            }
            #[cfg(feature = "count-allocations")]
            "--allocations" => {
                allocations::start();
                complete(&input, &mut output, chunk);
                let count = allocations::finish();
                println!(
                    "{name}\t{}\t{}\t{}\t{}",
                    count[0], count[1], count[2], count[3]
                );
            }
            _ => panic!("unsupported probe mode"),
        }
    }
}
