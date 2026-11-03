//! Bounded tokio-quiche transport for the existing Hyperium H3 application.
//!
//! The QUIC worker alone touches quiche. Application handles publish bounded
//! stream state, with real FIN/reset and backpressure preserved across the
//! worker boundary. No request-body or HTTP header interpretation lives here.
use bytes::{Buf, Bytes};
use h3::quic::{
    self, ConnectionErrorIncoming as ConnectionError, StreamErrorIncoming as StreamError, StreamId,
    WriteBuf,
};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_quiche::{
    ApplicationOverQuic, QuicResult,
    metrics::Metrics,
    quic::{HandshakeInfo, QuicheConnection},
    quiche,
};

pub use boring;
pub use tokio_quiche;
const CHUNK: usize = 16 * 1024;
const CANCELLED: u64 = 0x10c; // H3_REQUEST_CANCELLED

/// Hard bounds on application-side metadata and receive allocations. QUIC's
/// separate flow-control buffers must also be included in the node's budget.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub streams: usize,
    pub receive_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            streams: 256,
            receive_bytes: 256 * 1024,
        }
    }
}

struct State {
    streams: HashMap<u64, StreamState>,
    dirty: VecDeque<u64>,
    accept: [VecDeque<u64>; 2],
    accept_waker: [Option<Waker>; 2],
    pending_open: VecDeque<u64>,
    next: [u64; 2],
    error: Option<ConnectionError>,
    close: Option<(u64, Vec<u8>)>,
    established: bool,
}
struct StreamState {
    recv: VecDeque<Bytes>,
    recv_fin: bool,
    recv_error: Option<u64>,
    stop: Option<u64>,
    recv_dropped: bool,
    send: Option<WriteBuf<Bytes>>,
    finish: bool,
    sent_fin: bool,
    send_error: Option<u64>,
    reset: Option<u64>,
    send_dropped: bool,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    open_waker: Option<Waker>,
    opened: bool,
    claimed: bool,
    queued: bool,
}
impl StreamState {
    fn new(id: u64, local: bool, opened: bool) -> Self {
        let uni = id & 2 != 0;
        Self {
            recv: VecDeque::new(),
            recv_fin: uni && local,
            recv_error: None,
            stop: None,
            recv_dropped: uni && local,
            send: None,
            finish: false,
            sent_fin: uni && !local,
            send_error: None,
            reset: None,
            send_dropped: uni && !local,
            read_waker: None,
            write_waker: None,
            open_waker: None,
            opened,
            claimed: false,
            queued: false,
        }
    }
    fn wake(&mut self) {
        wake(&mut self.read_waker);
        wake(&mut self.write_waker);
        wake(&mut self.open_waker);
    }
}
fn wake(slot: &mut Option<Waker>) {
    if let Some(w) = slot.take() {
        w.wake();
    }
}
fn register(slot: &mut Option<Waker>, cx: &Context<'_>) {
    if !slot.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
        *slot = Some(cx.waker().clone());
    }
}
impl State {
    fn dirty(&mut self, id: u64) {
        if let Some(s) = self.streams.get_mut(&id) {
            if !s.queued {
                s.queued = true;
                self.dirty.push_back(id);
            }
        }
    }
    fn terminate(&mut self, error: ConnectionError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
        for w in &mut self.accept_waker {
            wake(w);
        }
        for s in self.streams.values_mut() {
            s.wake();
        }
    }
    fn check(&self) -> Result<(), StreamError> {
        self.error
            .clone()
            .map_or(Ok(()), |e| Err(stream_connection_error(e)))
    }
}
struct Shared {
    state: Mutex<State>,
    notify: Arc<Notify>,
    receive_budget: Arc<Semaphore>,
    limits: Limits,
    handles: AtomicUsize,
}
impl Shared {
    fn close(&self, code: u64, reason: &[u8]) {
        let mut state = self.state.lock();
        if state.close.is_none() {
            state.close = Some((code, reason[..reason.len().min(1024)].to_vec()));
        }
        state.terminate(ConnectionError::ApplicationClose { error_code: code });
        drop(state);
        self.notify.notify_one();
    }
}
// Only application handles count. The worker owns Shared but cannot keep an
// abandoned connection alive indefinitely. Queues contain IDs, never handles.
struct Lease(Arc<Shared>);
impl Lease {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.handles.fetch_add(1, Ordering::Relaxed);
        Self(shared.clone())
    }
}
impl Clone for Lease {
    fn clone(&self) -> Self {
        Self::new(&self.0)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        if self.0.handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.close(0x100, b"application handles released");
        }
    }
}
/// A close handle intentionally does not keep application ownership alive.
#[derive(Clone)]
pub struct CloseHandle(Arc<Shared>);
impl CloseHandle {
    pub fn close(&self, code: u64, reason: &[u8]) {
        self.0.close(code, reason);
    }
    pub fn buffered_receive_bytes(&self) -> usize {
        self.0.limits.receive_bytes - self.0.receive_budget.available_permits()
    }
}
/// QUIC worker callbacks. Construct together with its H3-facing connection.
pub struct Driver {
    shared: Arc<Shared>,
}
pub struct Connection {
    opener: Opener,
}
pub struct Opener {
    lease: Lease,
    pending: [Option<u64>; 2],
}
pub struct SendStream {
    lease: Lease,
    id: u64,
}
pub struct RecvStream {
    lease: Lease,
    id: u64,
}
pub struct BidiStream {
    send: SendStream,
    recv: RecvStream,
}

/// Server-side transport. Construction allocates no payload buffers.
pub fn server(limits: Limits) -> Result<(Driver, Connection, CloseHandle), &'static str> {
    if limits.streams < 8
        || limits.streams > 65536
        || limits.receive_bytes < CHUNK
        || limits.receive_bytes > u32::MAX as usize
    {
        return Err("invalid QUIC adapter bounds");
    }
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            streams: HashMap::new(),
            dirty: VecDeque::new(),
            accept: [VecDeque::new(), VecDeque::new()],
            accept_waker: [None, None],
            pending_open: VecDeque::new(),
            next: [1, 3],
            error: None,
            close: None,
            established: false,
        }),
        notify: Arc::new(Notify::new()),
        receive_budget: Arc::new(Semaphore::new(limits.receive_bytes)),
        limits,
        handles: AtomicUsize::new(0),
    });
    Ok((
        Driver {
            shared: shared.clone(),
        },
        Connection {
            opener: Opener {
                lease: Lease::new(&shared),
                pending: [None, None],
            },
        },
        CloseHandle(shared),
    ))
}
fn stream_connection_error(e: ConnectionError) -> StreamError {
    StreamError::ConnectionErrorIncoming {
        connection_error: e,
    }
}
fn missing_stream() -> StreamError {
    stream_connection_error(ConnectionError::InternalError(
        "stream ownership already released".into(),
    ))
}
fn stream_id(id: u64) -> StreamId {
    StreamId::try_from(id).expect("QUIC stream ID was validated")
}
impl Opener {
    fn open(&mut self, cx: &Context<'_>, kind: usize) -> Poll<Result<u64, StreamError>> {
        let shared = &self.lease.0;
        let mut state = shared.state.lock();
        state.check()?;
        if self.pending[kind].is_none() {
            if state.streams.len() >= shared.limits.streams {
                return Poll::Ready(Err(stream_connection_error(
                    ConnectionError::InternalError("local stream metadata limit".into()),
                )));
            }
            let id = state.next[kind];
            if StreamId::try_from(id).is_err() {
                return Poll::Ready(Err(missing_stream()));
            }
            state.next[kind] = id + 4;
            state.streams.insert(id, StreamState::new(id, true, false));
            state.pending_open.push_back(id);
            self.pending[kind] = Some(id);
            shared.notify.notify_one();
        }
        let id = self.pending[kind].expect("pending stream was assigned");
        let s = state.streams.get_mut(&id).ok_or_else(missing_stream)?;
        if s.opened {
            s.claimed = true;
            self.pending[kind] = None;
            Poll::Ready(Ok(id))
        } else {
            register(&mut s.open_waker, cx);
            Poll::Pending
        }
    }
}
impl Drop for Opener {
    fn drop(&mut self) {
        let mut state = self.lease.0.state.lock();
        for id in self.pending.iter().flatten() {
            // An unopened stream never reached the peer; discard its command.
            if let Some(s) = state.streams.get_mut(id) {
                if s.opened {
                    s.claimed = true;
                    s.recv_dropped = true;
                    s.send_dropped = true;
                    s.reset = Some(CANCELLED);
                    if id & 2 == 0 {
                        s.stop = Some(CANCELLED);
                    }
                    state.dirty(*id);
                } else {
                    state.streams.remove(id);
                }
            }
        }
        state
            .pending_open
            .retain(|id| !self.pending.contains(&Some(*id)));
        drop(state);
        self.lease.0.notify.notify_one();
    }
}
impl quic::OpenStreams<Bytes> for Opener {
    type BidiStream = BidiStream;
    type SendStream = SendStream;
    fn poll_open_bidi(&mut self, cx: &mut Context<'_>) -> Poll<Result<BidiStream, StreamError>> {
        self.open(cx, 0).map_ok(|id| BidiStream {
            send: SendStream {
                lease: self.lease.clone(),
                id,
            },
            recv: RecvStream {
                lease: self.lease.clone(),
                id,
            },
        })
    }
    fn poll_open_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<SendStream, StreamError>> {
        self.open(cx, 1).map_ok(|id| SendStream {
            lease: self.lease.clone(),
            id,
        })
    }
    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        self.lease.0.close(code.value(), reason);
    }
}
impl quic::OpenStreams<Bytes> for Connection {
    type BidiStream = BidiStream;
    type SendStream = SendStream;
    fn poll_open_bidi(&mut self, cx: &mut Context<'_>) -> Poll<Result<BidiStream, StreamError>> {
        self.opener.poll_open_bidi(cx)
    }
    fn poll_open_send(&mut self, cx: &mut Context<'_>) -> Poll<Result<SendStream, StreamError>> {
        self.opener.poll_open_send(cx)
    }
    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        self.opener.close(code, reason);
    }
}
impl Connection {
    fn accept(&mut self, cx: &Context<'_>, kind: usize) -> Poll<Result<u64, ConnectionError>> {
        let mut state = self.opener.lease.0.state.lock();
        if let Some(e) = &state.error {
            return Poll::Ready(Err(e.clone()));
        }
        if let Some(id) = state.accept[kind].pop_front() {
            state
                .streams
                .get_mut(&id)
                .expect("accept queue owns metadata")
                .claimed = true;
            Poll::Ready(Ok(id))
        } else {
            register(&mut state.accept_waker[kind], cx);
            Poll::Pending
        }
    }
}
impl quic::Connection<Bytes> for Connection {
    type RecvStream = RecvStream;
    type OpenStreams = Opener;
    fn poll_accept_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<RecvStream, ConnectionError>> {
        self.accept(cx, 1).map_ok(|id| RecvStream {
            lease: self.opener.lease.clone(),
            id,
        })
    }
    fn poll_accept_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<BidiStream, ConnectionError>> {
        self.accept(cx, 0).map_ok(|id| BidiStream {
            send: SendStream {
                lease: self.opener.lease.clone(),
                id,
            },
            recv: RecvStream {
                lease: self.opener.lease.clone(),
                id,
            },
        })
    }
    fn opener(&self) -> Opener {
        Opener {
            lease: self.opener.lease.clone(),
            pending: [None, None],
        }
    }
}
impl quic::SendStream<Bytes> for SendStream {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        let mut state = self.lease.0.state.lock();
        state.check()?;
        let s = state.streams.get_mut(&self.id).ok_or_else(missing_stream)?;
        if let Some(error_code) = s.send_error {
            return Poll::Ready(Err(StreamError::StreamTerminated { error_code }));
        }
        if s.finish {
            return Poll::Ready(Err(StreamError::StreamTerminated {
                error_code: CANCELLED,
            }));
        }
        if s.send.is_none() {
            Poll::Ready(Ok(()))
        } else {
            register(&mut s.write_waker, cx);
            Poll::Pending
        }
    }
    fn send_data<T: Into<WriteBuf<Bytes>>>(&mut self, data: T) -> Result<(), StreamError> {
        let mut state = self.lease.0.state.lock();
        state.check()?;
        let s = state.streams.get_mut(&self.id).ok_or_else(missing_stream)?;
        if let Some(error_code) = s.send_error {
            return Err(StreamError::StreamTerminated { error_code });
        }
        if s.send.is_some() || s.finish {
            return Err(stream_connection_error(ConnectionError::InternalError(
                "send_data without readiness".into(),
            )));
        }
        s.send = Some(data.into());
        state.dirty(self.id);
        drop(state);
        self.lease.0.notify.notify_one();
        Ok(())
    }
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        let mut state = self.lease.0.state.lock();
        state.check()?;
        let s = state.streams.get_mut(&self.id).ok_or_else(missing_stream)?;
        if let Some(error_code) = s.send_error {
            return Poll::Ready(Err(StreamError::StreamTerminated { error_code }));
        }
        if s.sent_fin {
            return Poll::Ready(Ok(()));
        }
        s.finish = true;
        register(&mut s.write_waker, cx);
        state.dirty(self.id);
        drop(state);
        self.lease.0.notify.notify_one();
        Poll::Pending
    }
    fn reset(&mut self, reset_code: u64) {
        let mut state = self.lease.0.state.lock();
        if let Some(s) = state.streams.get_mut(&self.id) {
            s.reset = Some(reset_code);
            s.send_error = Some(reset_code);
            s.send = None;
            state.dirty(self.id);
        }
        drop(state);
        self.lease.0.notify.notify_one();
    }
    fn send_id(&self) -> StreamId {
        stream_id(self.id)
    }
}
impl Drop for SendStream {
    fn drop(&mut self) {
        let mut state = self.lease.0.state.lock();
        if let Some(s) = state.streams.get_mut(&self.id) {
            s.send_dropped = true;
            if !s.sent_fin && s.send_error.is_none() {
                s.reset = Some(CANCELLED);
            }
            s.send = None;
            state.dirty(self.id);
        }
        drop(state);
        self.lease.0.notify.notify_one();
    }
}
impl quic::RecvStream for RecvStream {
    type Buf = Bytes;
    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, StreamError>> {
        let mut state = self.lease.0.state.lock();
        state.check()?;
        let s = state.streams.get_mut(&self.id).ok_or_else(missing_stream)?;
        if let Some(error_code) = s.recv_error {
            return Poll::Ready(Err(StreamError::StreamTerminated { error_code }));
        }
        if let Some(bytes) = s.recv.pop_front() {
            state.dirty(self.id);
            drop(state);
            self.lease.0.notify.notify_one();
            Poll::Ready(Ok(Some(bytes)))
        } else if s.recv_fin {
            Poll::Ready(Ok(None))
        } else {
            register(&mut s.read_waker, cx);
            Poll::Pending
        }
    }
    fn stop_sending(&mut self, error_code: u64) {
        let mut state = self.lease.0.state.lock();
        if let Some(s) = state.streams.get_mut(&self.id) {
            s.stop = Some(error_code);
            s.recv_error = Some(error_code);
            s.recv.clear();
            state.dirty(self.id);
        }
        drop(state);
        self.lease.0.notify.notify_one();
    }
    fn recv_id(&self) -> StreamId {
        stream_id(self.id)
    }
}
impl Drop for RecvStream {
    fn drop(&mut self) {
        let mut state = self.lease.0.state.lock();
        if let Some(s) = state.streams.get_mut(&self.id) {
            s.recv_dropped = true;
            if !s.recv_fin && s.recv_error.is_none() {
                s.stop = Some(CANCELLED);
            }
            s.recv.clear();
            state.dirty(self.id);
        }
        drop(state);
        self.lease.0.notify.notify_one();
    }
}
impl quic::SendStream<Bytes> for BidiStream {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        self.send.poll_ready(cx)
    }
    fn send_data<T: Into<WriteBuf<Bytes>>>(&mut self, data: T) -> Result<(), StreamError> {
        self.send.send_data(data)
    }
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamError>> {
        self.send.poll_finish(cx)
    }
    fn reset(&mut self, code: u64) {
        self.send.reset(code);
    }
    fn send_id(&self) -> StreamId {
        stream_id(self.send.id)
    }
}
impl quic::RecvStream for BidiStream {
    type Buf = Bytes;
    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Bytes>, StreamError>> {
        self.recv.poll_data(cx)
    }
    fn stop_sending(&mut self, code: u64) {
        self.recv.stop_sending(code);
    }
    fn recv_id(&self) -> StreamId {
        stream_id(self.recv.id)
    }
}
impl quic::BidiStream<Bytes> for BidiStream {
    type SendStream = SendStream;
    type RecvStream = RecvStream;
    fn split(self) -> (SendStream, RecvStream) {
        (self.send, self.recv)
    }
}

// Bytes clones retain the permit until the last clone disappears, including
// after H3 transfers ownership to the CDN body/cache path.
struct ReceiveAllocation {
    data: Vec<u8>,
    permit: OwnedSemaphorePermit,
    notify: Arc<Notify>,
}
impl AsRef<[u8]> for ReceiveAllocation {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}
impl Drop for ReceiveAllocation {
    fn drop(&mut self) {
        // Release before notifying: otherwise a worker can wake, find no budget,
        // and miss the only notification when the permit is dropped afterwards.
        drop(self.permit.split(self.permit.num_permits()));
        self.notify.notify_one();
    }
}
impl Driver {
    fn pump(&mut self, qconn: &mut QuicheConnection) -> QuicResult<()> {
        let mut state = self.shared.state.lock();
        if let Some((code, reason)) = state.close.take() {
            qconn.close(true, code, &reason)?;
            return Ok(());
        }
        if state.error.is_some() || !state.established {
            return Ok(());
        }
        // Only pending opens are retried on incoming MAX_STREAMS; normal traffic
        // visits QUIC's readable/writable sets and the deduplicated dirty queue.
        let pending = state.pending_open.len();
        for _ in 0..pending {
            let id = state
                .pending_open
                .pop_front()
                .expect("bounded pending length");
            if !state.streams.contains_key(&id) {
                continue;
            }
            match qconn.stream_send(id, &[], false) {
                Ok(_) => {
                    let s = state.streams.get_mut(&id).expect("present");
                    s.opened = true;
                    wake(&mut s.open_waker);
                    state.dirty(id);
                }
                Err(quiche::Error::Done | quiche::Error::StreamLimit) => {
                    state.pending_open.push_back(id)
                }
                Err(e) => return Err(e.into()),
            }
        }
        for id in qconn.readable() {
            if !state.streams.contains_key(&id) {
                if id & 1 != 0 {
                    return Err("unknown locally initiated readable stream".into());
                }
                if state.streams.len() >= self.shared.limits.streams {
                    return Err("peer stream metadata limit".into());
                }
                state.streams.insert(id, StreamState::new(id, false, true));
                let kind = usize::from(id & 2 != 0);
                state.accept[kind].push_back(id);
                wake(&mut state.accept_waker[kind]);
            }
            state.dirty(id);
        }
        for id in qconn.writable() {
            state.dirty(id);
        }
        let count = state.dirty.len();
        for _ in 0..count {
            let id = state.dirty.pop_front().expect("bounded dirty length");
            let Some(s) = state.streams.get_mut(&id) else {
                continue;
            };
            s.queued = false;
            if !s.opened {
                continue;
            }
            if let Some(code) = s.stop.take() {
                match qconn.stream_shutdown(id, quiche::Shutdown::Read, code) {
                    Ok(()) | Err(quiche::Error::Done) => {}
                    Err(e) => return Err(e.into()),
                }
                s.recv_fin = true;
                s.recv.clear();
            }
            if let Some(code) = s.reset.take() {
                match qconn.stream_shutdown(id, quiche::Shutdown::Write, code) {
                    Ok(()) | Err(quiche::Error::Done) => {}
                    Err(e) => return Err(e.into()),
                }
                s.sent_fin = true;
                s.send = None;
            }
            while !s.recv_fin
                && s.recv_error.is_none()
                && !s.recv_dropped
                && s.recv.len() < 2
                && qconn.stream_readable(id)
            {
                let Ok(permit) = self
                    .shared
                    .receive_budget
                    .clone()
                    .try_acquire_many_owned(CHUNK as u32)
                else {
                    break;
                };
                let mut allocation = ReceiveAllocation {
                    data: vec![0; CHUNK],
                    permit,
                    notify: self.shared.notify.clone(),
                };
                match qconn.stream_recv(id, &mut allocation.data) {
                    Ok((n, fin)) => {
                        allocation.data.truncate(n);
                        s.recv_fin = fin;
                        if n > 0 {
                            s.recv.push_back(Bytes::from_owner(allocation));
                        }
                        wake(&mut s.read_waker);
                        if n == 0 {
                            break;
                        }
                    }
                    Err(quiche::Error::Done) => break,
                    Err(quiche::Error::StreamReset(code)) => {
                        s.recv_error = Some(code);
                        s.recv.clear();
                        wake(&mut s.read_waker);
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let mut again = false;
            if let Some(buf) = s.send.as_mut() {
                for _ in 0..4 {
                    if !buf.has_remaining() {
                        break;
                    }
                    match qconn.stream_send(id, buf.chunk(), false) {
                        Ok(n) => {
                            buf.advance(n);
                            if n == 0 {
                                break;
                            }
                            again = buf.has_remaining();
                        }
                        Err(quiche::Error::Done) => {
                            again = false;
                            break;
                        }
                        Err(quiche::Error::StreamStopped(code)) => {
                            s.send_error = Some(code);
                            again = false;
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                if !buf.has_remaining() || s.send_error.is_some() {
                    s.send = None;
                    wake(&mut s.write_waker);
                }
            }
            if s.finish && s.send.is_none() && !s.sent_fin && s.send_error.is_none() {
                match qconn.stream_send(id, &[], true) {
                    Ok(_) => {
                        s.sent_fin = true;
                        wake(&mut s.write_waker);
                    }
                    Err(quiche::Error::Done) => {}
                    Err(quiche::Error::StreamStopped(code)) => {
                        s.send_error = Some(code);
                        wake(&mut s.write_waker);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            if s.claimed && s.recv_dropped && s.send_dropped {
                state.streams.remove(&id);
            } else if again {
                state.dirty(id);
                self.shared.notify.notify_one();
            }
        }
        Ok(())
    }
}
impl ApplicationOverQuic for Driver {
    fn on_conn_established(
        &mut self,
        _: &mut QuicheConnection,
        _: &HandshakeInfo,
    ) -> QuicResult<()> {
        self.shared.state.lock().established = true;
        self.shared.notify.notify_one();
        Ok(())
    }
    fn should_act(&self) -> bool {
        true
    }
    async fn wait_for_data(&mut self, _: &mut QuicheConnection) -> QuicResult<()> {
        self.shared.notify.notified().await;
        Ok(())
    }
    fn process_reads(&mut self, qconn: &mut QuicheConnection) -> QuicResult<()> {
        self.pump(qconn)
    }
    fn process_writes(&mut self, qconn: &mut QuicheConnection) -> QuicResult<()> {
        self.pump(qconn)
    }
    fn on_conn_close<M: Metrics>(
        &mut self,
        qconn: &mut QuicheConnection,
        _: &M,
        result: &QuicResult<()>,
    ) {
        let error = if qconn.is_timed_out() {
            ConnectionError::Timeout
        } else if let Some(error) = qconn.peer_error() {
            if error.is_app {
                ConnectionError::ApplicationClose {
                    error_code: error.error_code,
                }
            } else {
                ConnectionError::InternalError(format!("QUIC transport close: {error:?}"))
            }
        } else {
            ConnectionError::InternalError(format!("QUIC worker ended: {result:?}"))
        };
        let mut state = self.shared.state.lock();
        state.terminate(error);
        state.streams.clear();
        state.dirty.clear();
        state.pending_open.clear();
        state.accept.iter_mut().for_each(VecDeque::clear);
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.shared
            .state
            .lock()
            .terminate(ConnectionError::InternalError("QUIC worker dropped".into()));
    }
}
