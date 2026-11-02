#![cfg(all(target_os = "linux", feature = "io-uring"))]

use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zelvik_edge_io::splice::{Direction, Observer};
use zelvik_edge_io::uring::{Config, Driver, EndReason, Handle, NoopObserver, Transfer};

async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (client.unwrap(), accepted.unwrap().0)
}

async fn connection<O: Observer + Send + 'static>(
    handle: &Handle<O>,
    forward: O,
    reverse: O,
) -> (Transfer, TcpStream, TcpStream) {
    let (client, front) = pair().await;
    let (back, origin) = pair().await;
    let transfer = handle
        .start(
            front.into_std().unwrap(),
            back.into_std().unwrap(),
            forward,
            reverse,
        )
        .await
        .unwrap();
    (transfer, client, origin)
}

async fn eof(socket: &mut TcpStream) {
    let mut byte = [0; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

struct Progress {
    bytes: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
    idle: Option<Duration>,
}

impl Observer for Progress {
    fn transferred(&mut self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    fn read_idle_timeout(&self) -> Option<Duration> {
        self.idle
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

fn progress(idle: Option<Duration>) -> (Progress, Arc<AtomicU64>, Arc<AtomicBool>) {
    let bytes = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    (
        Progress {
            bytes: bytes.clone(),
            dropped: dropped.clone(),
            idle,
        },
        bytes,
        dropped,
    )
}

#[tokio::test]
async fn half_close_keeps_the_final_origin_response_and_flushes_observers() {
    let mut driver = Driver::<Progress>::new(Config::default()).unwrap();
    let (forward, sent, forward_drop) = progress(None);
    let (reverse, received, reverse_drop) = progress(None);
    let (transfer, mut client, mut origin) = connection(&driver.handle(), forward, reverse).await;
    client.write_all(b"request").await.unwrap();
    client.shutdown().await.unwrap();
    let mut request = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), origin.read_to_end(&mut request))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request, b"request");
    origin.write_all(b"final-response").await.unwrap();
    origin.shutdown().await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, b"final-response");
    let outcome = tokio::time::timeout(Duration::from_secs(5), transfer)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome.reason, EndReason::Clean));
    assert_eq!((outcome.forward_bytes, outcome.reverse_bytes), (7, 14));
    assert_eq!(
        (
            sent.load(Ordering::Relaxed),
            received.load(Ordering::Relaxed)
        ),
        (7, 14)
    );
    assert!(forward_drop.load(Ordering::Acquire) && reverse_drop.load(Ordering::Acquire));
    assert!(outcome.pipe_capacities.iter().all(|p| p.actual > 0));
    let report = driver.shutdown().unwrap().unwrap();
    assert_eq!(
        (
            report.accepted,
            report.completed,
            report.abandoned_receivers
        ),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn cancellation_drains_write_completions_and_releases_sockets() {
    let mut driver = Driver::<Progress>::new(Config::default()).unwrap();
    let (forward, sent, forward_drop) = progress(None);
    let (reverse, received, reverse_drop) = progress(None);
    let (transfer, mut client, mut origin) = connection(&driver.handle(), forward, reverse).await;
    let payload = vec![0x5a; 32 * 1024];
    client.write_all(&payload).await.unwrap();
    let mut bytes = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(5), origin.read_exact(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, payload);
    transfer.cancel().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), transfer)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome.reason, EndReason::Cancelled));
    assert_eq!(
        (outcome.forward_bytes, outcome.reverse_bytes),
        (payload.len() as u64, 0)
    );
    assert_eq!(
        (
            sent.load(Ordering::Relaxed),
            received.load(Ordering::Relaxed)
        ),
        (payload.len() as u64, 0)
    );
    assert!(forward_drop.load(Ordering::Acquire) && reverse_drop.load(Ordering::Acquire));
    eof(&mut client).await;
    eof(&mut origin).await;
    assert_eq!(driver.shutdown().unwrap().unwrap().completed, 1);
}

#[tokio::test]
async fn dropping_an_idle_transfer_leaves_no_socket_pinned_in_the_ring() {
    let mut driver = Driver::<Progress>::new(Config::default()).unwrap();
    let (forward, _, forward_drop) = progress(None);
    let (reverse, _, reverse_drop) = progress(None);
    let (transfer, mut client, mut origin) = connection(&driver.handle(), forward, reverse).await;
    drop(transfer);
    eof(&mut client).await;
    eof(&mut origin).await;
    assert!(forward_drop.load(Ordering::Acquire) && reverse_drop.load(Ordering::Acquire));
    let report = driver.shutdown().unwrap().unwrap();
    assert_eq!(
        (
            report.accepted,
            report.completed,
            report.abandoned_receivers
        ),
        (1, 1, 1)
    );
}

#[tokio::test]
async fn tiny_submission_ring_flushes_ready_work_behind_idle_polls() {
    let mut driver = Driver::<NoopObserver>::new(Config {
        ring_entries: 2,
        max_connections: 16,
        ..Config::default()
    })
    .unwrap();
    let handle = driver.handle();
    let (idle, mut idle_client, mut idle_origin) =
        connection(&handle, NoopObserver, NoopObserver).await;
    let (hot, mut client, mut origin) = connection(&handle, NoopObserver, NoopObserver).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        client.write_all(b"active").await.unwrap();
        let mut request = [0; 6];
        origin.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"active");
        origin.write_all(b"reply").await.unwrap();
        let mut response = [0; 5];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"reply");
        client.shutdown().await.unwrap();
        origin.shutdown().await.unwrap();
        let result = hot.await.unwrap();
        assert!(matches!(result.reason, EndReason::Clean));
        assert_eq!((result.forward_bytes, result.reverse_bytes), (6, 5));
    })
    .await
    .unwrap();
    idle.cancel().unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), idle)
            .await
            .unwrap()
            .unwrap()
            .reason,
        EndReason::Cancelled
    ));
    eof(&mut idle_client).await;
    eof(&mut idle_origin).await;
    assert_eq!(driver.shutdown().unwrap().unwrap().completed, 2);
}

#[tokio::test]
async fn idle_deadline_is_observable_and_cancels_the_other_direction() {
    let mut driver = Driver::<Progress>::new(Config::default()).unwrap();
    let (forward, _, forward_drop) = progress(Some(Duration::from_millis(20)));
    let (reverse, _, reverse_drop) = progress(None);
    let (transfer, mut client, mut origin) = connection(&driver.handle(), forward, reverse).await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), transfer)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        outcome.reason,
        EndReason::IdleTimeout(Direction::Forward)
    ));
    assert_eq!((outcome.forward_bytes, outcome.reverse_bytes), (0, 0));
    assert!(forward_drop.load(Ordering::Acquire) && reverse_drop.load(Ordering::Acquire));
    eof(&mut client).await;
    eof(&mut origin).await;
    assert_eq!(driver.shutdown().unwrap().unwrap().completed, 1);
}

#[tokio::test]
async fn shutdown_cancels_every_active_connection_and_closes_admission() {
    let mut driver = Driver::<NoopObserver>::new(Config {
        ring_entries: 4,
        max_connections: 16,
        ..Config::default()
    })
    .unwrap();
    let handle = driver.handle();
    let mut active = Vec::new();
    for _ in 0..8 {
        let (transfer, mut client, mut origin) =
            connection(&handle, NoopObserver, NoopObserver).await;
        client.write_all(b"x").await.unwrap();
        let mut byte = [0];
        tokio::time::timeout(Duration::from_secs(5), origin.read_exact(&mut byte))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(byte, [b'x']);
        active.push((transfer, client, origin));
    }
    let report = driver.shutdown().unwrap().unwrap();
    assert_eq!(
        (
            report.accepted,
            report.completed,
            report.abandoned_receivers
        ),
        (8, 8, 0)
    );
    for (transfer, mut client, mut origin) in active {
        let outcome = transfer.await.unwrap();
        assert!(matches!(outcome.reason, EndReason::Cancelled));
        assert_eq!((outcome.forward_bytes, outcome.reverse_bytes), (1, 0));
        eof(&mut client).await;
        eof(&mut origin).await;
    }
    let (_client, front) = pair().await;
    let (back, _origin) = pair().await;
    let rejected = handle
        .start(
            front.into_std().unwrap(),
            back.into_std().unwrap(),
            NoopObserver,
            NoopObserver,
        )
        .await;
    assert!(matches!(rejected, Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe));
    assert!(driver.shutdown().unwrap().is_none());
}

#[tokio::test]
async fn large_payloads_drain_small_pipes_and_return_exactly() {
    let mut driver = Driver::<NoopObserver>::new(Config {
        ring_entries: 4,
        pipe_capacity_hint: 4096,
        ..Config::default()
    })
    .unwrap();
    let (transfer, mut client, mut origin) =
        connection(&driver.handle(), NoopObserver, NoopObserver).await;
    let payload = vec![0x6b; 512 * 1024];
    tokio::time::timeout(Duration::from_secs(10), async {
        let client_side = async {
            client.write_all(&payload).await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, payload);
        };
        let origin_side = async {
            let mut request = Vec::new();
            origin.read_to_end(&mut request).await.unwrap();
            assert_eq!(request, payload);
            origin.write_all(&request).await.unwrap();
            origin.shutdown().await.unwrap();
        };
        let ((), (), outcome) = tokio::join!(client_side, origin_side, transfer);
        let outcome = outcome.unwrap();
        assert!(matches!(outcome.reason, EndReason::Clean));
        assert_eq!(
            (outcome.forward_bytes, outcome.reverse_bytes),
            (payload.len() as u64, payload.len() as u64)
        );
    })
    .await
    .unwrap();
    assert_eq!(driver.shutdown().unwrap().unwrap().completed, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_connections_progress_on_one_owned_worker() {
    let mut driver = Driver::<NoopObserver>::new(Config {
        ring_entries: 8,
        max_connections: 32,
        ..Config::default()
    })
    .unwrap();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let handle = driver.handle();
        tasks.spawn(async move {
            let (transfer, mut client, mut origin) =
                connection(&handle, NoopObserver, NoopObserver).await;
            let client_side = async {
                for _ in 0..16 {
                    client.write_all(b"request").await.unwrap();
                    let mut response = [0; 8];
                    client.read_exact(&mut response).await.unwrap();
                    assert_eq!(&response, b"response");
                }
                client.shutdown().await.unwrap();
            };
            let origin_side = async {
                for _ in 0..16 {
                    let mut request = [0; 7];
                    origin.read_exact(&mut request).await.unwrap();
                    assert_eq!(&request, b"request");
                    origin.write_all(b"response").await.unwrap();
                }
                origin.shutdown().await.unwrap();
            };
            let ((), (), outcome) = tokio::join!(client_side, origin_side, transfer);
            let outcome = outcome.unwrap();
            assert!(matches!(outcome.reason, EndReason::Clean));
            assert_eq!(
                (outcome.forward_bytes, outcome.reverse_bytes),
                (7 * 16, 8 * 16)
            );
        });
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .unwrap();
    let report = driver.shutdown().unwrap().unwrap();
    assert_eq!(
        (report.accepted, report.completed, report.rejected),
        (16, 16, 0)
    );
}

#[tokio::test]
async fn capacity_rejection_keeps_existing_connections_live() {
    let mut driver = Driver::<NoopObserver>::new(Config {
        max_connections: 1,
        ..Config::default()
    })
    .unwrap();
    let handle = driver.handle();
    let (first, mut client, mut origin) = connection(&handle, NoopObserver, NoopObserver).await;
    client.write_all(b"a").await.unwrap();
    let mut byte = [0];
    tokio::time::timeout(Duration::from_secs(5), origin.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, [b'a']);
    let (rejected, mut other_client, mut other_origin) =
        connection(&handle, NoopObserver, NoopObserver).await;
    let error = tokio::time::timeout(Duration::from_secs(5), rejected)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    eof(&mut other_client).await;
    eof(&mut other_origin).await;
    client.write_all(b"b").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), origin.read_exact(&mut byte))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(byte, [b'b']);
    first.cancel().unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome.reason, EndReason::Cancelled));
    assert_eq!((outcome.forward_bytes, outcome.reverse_bytes), (2, 0));
    let report = driver.shutdown().unwrap().unwrap();
    assert_eq!(
        (report.accepted, report.completed, report.rejected),
        (1, 1, 1)
    );
}

#[tokio::test]
async fn origin_reset_remains_an_observable_io_error() {
    let mut driver = Driver::<NoopObserver>::new(Config::default()).unwrap();
    let (transfer, _client, origin) =
        connection(&driver.handle(), NoopObserver, NoopObserver).await;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: origin owns a live socket; linger points to the correctly
    // sized value for SO_LINGER and stays alive through setsockopt.
    let result = unsafe {
        libc::setsockopt(
            origin.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&linger as *const libc::linger).cast(),
            std::mem::size_of_val(&linger) as libc::socklen_t,
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    drop(origin);
    let outcome = tokio::time::timeout(Duration::from_secs(5), transfer)
        .await
        .unwrap()
        .unwrap();
    let EndReason::Failed(error) = outcome.reason else {
        panic!("reset was not reported as a failure");
    };
    assert_eq!(error.source.kind(), std::io::ErrorKind::ConnectionReset);
    assert_eq!(driver.shutdown().unwrap().unwrap().completed, 1);
}
