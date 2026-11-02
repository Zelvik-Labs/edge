pub mod blocking;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use zelvik_edge_io::splice::{Observer, PreparedSplice};

#[derive(Clone, Copy)]
pub enum Engine {
    Blocking,
    Readiness,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Readiness => "readiness",
        }
    }
}

struct Count {
    value: Arc<AtomicU64>,
    total: u64,
}

impl Observer for Count {
    fn transferred(&mut self, bytes: usize) {
        self.total += bytes as u64;
        self.value.store(self.total, Ordering::Relaxed);
    }
}

pub async fn relay(engine: Engine, front: &TcpStream, back: &TcpStream) -> io::Result<(u64, u64)> {
    match engine {
        Engine::Blocking => blocking::relay(front, back).await,
        Engine::Readiness => {
            let sent = Arc::new(AtomicU64::new(0));
            let received = Arc::new(AtomicU64::new(0));
            PreparedSplice::new(256 * 1024)?
                .run(
                    front,
                    back,
                    Count {
                        value: sent.clone(),
                        total: 0,
                    },
                    Count {
                        value: received.clone(),
                        total: 0,
                    },
                )
                .await
                .map_err(io::Error::other)?;
            Ok((
                sent.load(Ordering::Relaxed),
                received.load(Ordering::Relaxed),
            ))
        }
    }
}

pub async fn pair() -> io::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let (client, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr()?),
        listener.accept()
    );
    let (client, accepted) = (client?, accepted?.0);
    client.set_nodelay(true)?;
    accepted.set_nodelay(true)?;
    Ok((client, accepted))
}

pub struct Connection {
    client: Mutex<(TcpStream, Vec<u8>)>,
    payload: Vec<u8>,
    completed: AtomicU64,
    relay: tokio::task::JoinHandle<io::Result<(u64, u64)>>,
    origin: tokio::task::JoinHandle<io::Result<u64>>,
}

impl Connection {
    pub async fn new(engine: Engine, bytes: usize) -> Self {
        let (client, front) = pair().await.unwrap();
        let (back, mut origin) = pair().await.unwrap();
        let transfer = tokio::spawn(async move { relay(engine, &front, &back).await });
        let echo = tokio::spawn(async move {
            let mut buffer = vec![0; bytes];
            let mut total = 0;
            loop {
                if origin.read(&mut buffer[..1]).await? == 0 {
                    break;
                }
                origin.read_exact(&mut buffer[1..]).await?;
                if buffer.iter().any(|b| *b != 0x5a) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "origin payload mismatch",
                    ));
                }
                origin.write_all(&buffer).await?;
                total += bytes as u64;
            }
            origin.shutdown().await?;
            Ok(total)
        });
        Self {
            client: Mutex::new((client, vec![0; bytes])),
            payload: vec![0x5a; bytes],
            completed: AtomicU64::new(0),
            relay: transfer,
            origin: echo,
        }
    }

    pub async fn exchange(&self) {
        let mut guard = self.client.lock().await;
        let (client, response) = &mut *guard;
        client.write_all(&self.payload).await.unwrap();
        client.read_exact(response).await.unwrap();
        assert_eq!(response.as_slice(), self.payload.as_slice());
        self.completed.fetch_add(1, Ordering::Relaxed);
    }

    pub async fn finish(self) {
        let (mut client, _) = self.client.into_inner();
        client.shutdown().await.unwrap();
        let expected = self.completed.load(Ordering::Relaxed) * self.payload.len() as u64;
        let stats = self.relay.await.unwrap().unwrap();
        let origin_bytes = self.origin.await.unwrap().unwrap();
        assert_eq!(stats, (expected, expected));
        assert_eq!(origin_bytes, expected);
    }
}
