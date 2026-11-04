#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zelvik_edge_io::splice::{Observer, Operation, PreparedSplice};

async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    (client.unwrap(), accepted.unwrap().0)
}

struct Progress {
    count: Arc<AtomicU64>,
    dropped: Arc<AtomicBool>,
    timeout: Option<Duration>,
}

impl Observer for Progress {
    fn transferred(&mut self, bytes: usize) {
        self.count.fetch_add(bytes as u64, Ordering::Relaxed);
    }
    fn read_idle_timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Relaxed);
    }
}

fn progress(timeout: Option<Duration>) -> (Progress, Arc<AtomicU64>, Arc<AtomicBool>) {
    let count = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    (
        Progress {
            count: count.clone(),
            dropped: dropped.clone(),
            timeout,
        },
        count,
        dropped,
    )
}

#[test]
fn duplex_progress_does_not_require_a_blocking_worker() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (mut client, front) = pair().await;
        let (back, mut origin) = pair().await;
        let (forward, sent, _) = progress(None);
        let (reverse, received, _) = progress(None);
        let prepared = PreparedSplice::new(256 * 1024).unwrap();
        let exchange = async {
            let transfer = prepared.run(&front, &back, forward, reverse);
            let client = async {
                client.write_all(b"request").await.unwrap();
                let mut response = [0; 8];
                client.read_exact(&mut response).await.unwrap();
                assert_eq!(&response, b"response");
                client.shutdown().await.unwrap();
            };
            let origin = async {
                let mut request = [0; 7];
                origin.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"request");
                origin.write_all(b"response").await.unwrap();
                origin.shutdown().await.unwrap();
            };
            let (result, (), ()) = tokio::join!(transfer, client, origin);
            result.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), exchange)
            .await
            .unwrap();
        assert_eq!(sent.load(Ordering::Relaxed), 7);
        assert_eq!(received.load(Ordering::Relaxed), 8);
    });
}

#[tokio::test]
async fn client_half_close_preserves_the_delayed_origin_response() {
    let (mut client, front) = pair().await;
    let (back, mut origin) = pair().await;
    let (forward, sent, _) = progress(None);
    let (reverse, received, _) = progress(None);
    let relay = tokio::spawn(async move {
        PreparedSplice::new(256 * 1024)
            .unwrap()
            .run(&front, &back, forward, reverse)
            .await
            .unwrap();
    });
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
    relay.await.unwrap();
    assert_eq!(sent.load(Ordering::Relaxed), request.len() as u64);
    assert_eq!(received.load(Ordering::Relaxed), response.len() as u64);
}

#[tokio::test]
async fn abort_closes_owned_sockets_and_drops_both_observers() {
    let (mut client, front) = pair().await;
    let (back, mut origin) = pair().await;
    let (forward, sent, forward_dropped) = progress(None);
    let (reverse, _, reverse_dropped) = progress(None);
    let relay = tokio::spawn(async move {
        PreparedSplice::new(256 * 1024)
            .unwrap()
            .run(&front, &back, forward, reverse)
            .await
    });
    let body = vec![0x5a; 32 * 1024];
    client.write_all(&body).await.unwrap();
    let mut received = vec![0; body.len()];
    tokio::time::timeout(Duration::from_secs(5), origin.read_exact(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, body);
    relay.abort();
    assert!(relay.await.unwrap_err().is_cancelled());
    assert_eq!(sent.load(Ordering::Relaxed), body.len() as u64);
    assert!(forward_dropped.load(Ordering::Relaxed));
    assert!(reverse_dropped.load(Ordering::Relaxed));
    let mut byte = [0; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), origin.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn read_deadline_remains_an_observable_read_error() {
    let (_client, front) = pair().await;
    let (back, _origin) = pair().await;
    let (forward, _, _) = progress(Some(Duration::from_millis(20)));
    let (reverse, _, _) = progress(Some(Duration::from_millis(20)));
    let result = PreparedSplice::new(256 * 1024)
        .unwrap()
        .run(&front, &back, forward, reverse)
        .await
        .unwrap_err();
    assert_eq!(result.operation, Operation::Read);
    assert_eq!(result.source.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn payloads_larger_than_the_pipe_drain_exactly_in_both_directions() {
    let (mut client, front) = pair().await;
    let (back, mut origin) = pair().await;
    let (forward, sent, _) = progress(None);
    let (reverse, received, _) = progress(None);
    let payload = vec![0x6b; 512 * 1024];
    let relay = tokio::spawn(async move {
        PreparedSplice::new(4096)
            .unwrap()
            .run(&front, &back, forward, reverse)
            .await
            .unwrap();
    });
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
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(client_side, origin_side);
        relay.await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(sent.load(Ordering::Relaxed), payload.len() as u64);
    assert_eq!(received.load(Ordering::Relaxed), payload.len() as u64);
}
