#[path = "../../../../crates/zelvik-edge-io/benches/support/blocking.rs"]
mod blocking;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use zelvik_edge_io::splice::{Observer, PreparedSplice};

struct Count(Arc<AtomicU64>);
impl Observer for Count {
    fn transferred(&mut self, bytes: usize) {
        self.0.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

async fn pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let (client, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr()?),
        listener.accept()
    );
    Ok((client?, accepted?.0))
}

async fn relay(old: bool, front: &TcpStream, back: &TcpStream) -> io::Result<(u64, u64)> {
    if old {
        return blocking::relay(front, back).await;
    }
    let sent = Arc::new(AtomicU64::new(0));
    let received = Arc::new(AtomicU64::new(0));
    PreparedSplice::new(256 * 1024)?
        .run(front, back, Count(sent.clone()), Count(received.clone()))
        .await
        .map_err(io::Error::other)?;
    Ok((
        sent.load(Ordering::Relaxed),
        received.load(Ordering::Relaxed),
    ))
}

async fn scenario(old: bool, half_close: bool) -> io::Result<(u64, u64)> {
    let (mut client, front) = pair().await?;
    let (back, mut origin) = pair().await?;
    let response: &[u8] = if half_close {
        b"final-response"
    } else {
        b"response"
    };
    let exchange = async {
        let client_side = async {
            client.write_all(b"request").await?;
            if half_close {
                client.shutdown().await?;
            }
            let mut received = vec![0; response.len()];
            client.read_exact(&mut received).await?;
            if received != response {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "response mismatch",
                ));
            }
            client.shutdown().await?;
            Ok(())
        };
        let origin_side = async {
            let mut request = Vec::new();
            if half_close {
                origin.read_to_end(&mut request).await?;
            } else {
                request.resize(7, 0);
                origin.read_exact(&mut request).await?;
            }
            if request != b"request" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request mismatch",
                ));
            }
            origin.write_all(response).await?;
            origin.shutdown().await?;
            Ok(())
        };
        let (counts, (), ()) =
            tokio::try_join!(relay(old, &front, &back), client_side, origin_side)?;
        if counts != (7, response.len() as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("count mismatch: {counts:?}"),
            ));
        }
        Ok(counts)
    };
    match tokio::time::timeout(Duration::from_secs(2), exchange).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "duplex exchange exceeded 2s",
        )),
    }
}

fn proc_count(path: &str) -> io::Result<usize> {
    std::fs::read_dir(path)?.try_fold(0, |count, entry| entry.map(|_| count + 1))
}

async fn resources(old: bool) -> io::Result<serde_json::Value> {
    const CONNECTIONS: usize = 16;
    let base_fds = proc_count("/proc/self/fd")?;
    let base_threads = proc_count("/proc/self/task")?;
    let mut connections = Vec::new();
    for _ in 0..CONNECTIONS {
        let (mut client, front) = pair().await?;
        let (back, mut origin) = pair().await?;
        let task = tokio::spawn(async move { relay(old, &front, &back).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            client.write_all(b"x").await?;
            let mut byte = [0; 1];
            origin.read_exact(&mut byte).await?;
            if byte != [b'x'] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resource request mismatch",
                ));
            }
            origin.write_all(b"y").await?;
            client.read_exact(&mut byte).await?;
            if byte != [b'y'] {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resource response mismatch",
                ));
            }
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(io::Error::other)??;
        connections.push((client, origin, task));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let fds = proc_count("/proc/self/fd")?;
    let threads = proc_count("/proc/self/task")?;
    for (mut client, origin, task) in connections {
        client.shutdown().await?;
        drop(client);
        drop(origin);
        let counts = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)??;
        if counts != (1, 1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resource relay count mismatch",
            ));
        }
    }
    Ok(serde_json::json!({
        "connections": CONNECTIONS, "base_fds": base_fds, "fds": fds,
        "fd_delta": fds - base_fds, "base_threads": base_threads,
        "threads": threads, "thread_delta": threads - base_threads
    }))
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        3,
        "usage: probe blocking|readiness pool|half-close|resources"
    );
    let old = match args[1].as_str() {
        "blocking" => true,
        "readiness" => false,
        _ => panic!("unknown engine"),
    };
    let resource_probe = args[2] == "resources";
    let half_close = match args[2].as_str() {
        "pool" => false,
        "half-close" | "resources" => true,
        _ => panic!("unknown scenario"),
    };
    let blocking_threads = if resource_probe {
        64
    } else if half_close {
        4
    } else {
        1
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(blocking_threads)
        .enable_all()
        .build()
        .unwrap();
    let start = Instant::now();
    let result = runtime.block_on(async {
        if resource_probe {
            resources(old).await
        } else {
            scenario(old, half_close)
                .await
                .map(|counts| serde_json::json!(counts))
        }
    });
    let passed = result.is_ok();
    let counts = result.as_ref().ok().cloned();
    let error = result.err().map(|e| format!("{:?}: {e}", e.kind()));
    runtime.shutdown_timeout(Duration::from_secs(2));
    println!(
        "{}",
        serde_json::json!({
            "engine": args[1], "scenario": args[2], "max_blocking_threads": blocking_threads,
            "passed": passed, "counts": counts, "error": error,
            "elapsed_ms": start.elapsed().as_secs_f64() * 1000.0
        })
    );
    if !passed {
        std::process::exit(1);
    }
}
