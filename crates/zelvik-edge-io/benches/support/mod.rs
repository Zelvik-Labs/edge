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
    #[cfg(feature = "io-uring")]
    Uring,
    #[cfg(feature = "io-uring")]
    UringUnbounded,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Readiness => "readiness",
            #[cfg(feature = "io-uring")]
            Self::Uring => "uring",
            #[cfg(feature = "io-uring")]
            Self::UringUnbounded => "uring-unbounded",
        }
    }
}

struct Count {
    value: Arc<AtomicU64>,
    total: u64,
}

#[derive(Clone)]
struct Context {
    engine: Engine,
    #[cfg(feature = "io-uring")]
    uring: Option<zelvik_edge_io::uring::Handle<Count>>,
}

impl Observer for Count {
    fn transferred(&mut self, bytes: usize) {
        self.total += bytes as u64;
        self.value.store(self.total, Ordering::Relaxed);
    }
}

async fn relay(context: Context, front: TcpStream, back: TcpStream) -> io::Result<(u64, u64)> {
    match context.engine {
        Engine::Blocking => blocking::relay(&front, &back).await,
        Engine::Readiness => {
            let sent = Arc::new(AtomicU64::new(0));
            let received = Arc::new(AtomicU64::new(0));
            PreparedSplice::new(256 * 1024)?
                .run(
                    &front,
                    &back,
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
        #[cfg(feature = "io-uring")]
        Engine::Uring | Engine::UringUnbounded => {
            use zelvik_edge_io::uring::EndReason;
            let handle = context
                .uring
                .ok_or_else(|| io::Error::other("missing shared io_uring handle"))?;
            let forward = Count {
                value: Arc::new(AtomicU64::new(0)),
                total: 0,
            };
            let reverse = Count {
                value: Arc::new(AtomicU64::new(0)),
                total: 0,
            };
            let outcome = handle
                .start(front.into_std()?, back.into_std()?, forward, reverse)
                .await?
                .await?;
            match outcome.reason {
                EndReason::Clean => Ok((outcome.forward_bytes, outcome.reverse_bytes)),
                reason => Err(io::Error::other(format!(
                    "io_uring fixture ended: {reason:?}"
                ))),
            }
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

struct Connection {
    client: Mutex<(TcpStream, Vec<u8>)>,
    payload: Vec<u8>,
    completed: AtomicU64,
    relay: tokio::task::JoinHandle<io::Result<(u64, u64)>>,
    origin: tokio::task::JoinHandle<io::Result<u64>>,
}

impl Connection {
    async fn new(context: Context, bytes: usize) -> Self {
        let (client, front) = pair().await.unwrap();
        let (back, mut origin) = pair().await.unwrap();
        let transfer = tokio::spawn(async move { relay(context, front, back).await });
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

pub struct Group {
    engine: Engine,
    connections: Vec<Connection>,
    #[cfg(feature = "io-uring")]
    driver: Option<zelvik_edge_io::uring::Driver<Count>>,
}

impl Group {
    pub async fn new(engine: Engine, bytes: usize, concurrency: usize) -> Self {
        assert!([1, 16].contains(&concurrency));
        #[cfg(feature = "io-uring")]
        let driver = if matches!(engine, Engine::Uring | Engine::UringUnbounded) {
            let mut config = zelvik_edge_io::uring::Config::default();
            if matches!(engine, Engine::UringUnbounded) {
                config.io_wq_max_workers = None;
            }
            eprintln!("SPLICE_URING_CONFIG {config:?}");
            Some(zelvik_edge_io::uring::Driver::<Count>::new(config).unwrap())
        } else {
            None
        };
        let context = Context {
            engine,
            #[cfg(feature = "io-uring")]
            uring: driver.as_ref().map(|worker| worker.handle()),
        };
        let mut connections = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            connections.push(Connection::new(context.clone(), bytes).await);
        }
        Self {
            engine,
            connections,
            #[cfg(feature = "io-uring")]
            driver,
        }
    }

    pub async fn exchange(&self) {
        let c = &self.connections;
        if c.len() == 1 {
            c[0].exchange().await;
        } else {
            // Fixed batch: no task creation, Arc cloning or Vec allocation
            // is added to the measured iteration for either engine.
            tokio::join!(
                c[0].exchange(),
                c[1].exchange(),
                c[2].exchange(),
                c[3].exchange(),
                c[4].exchange(),
                c[5].exchange(),
                c[6].exchange(),
                c[7].exchange(),
                c[8].exchange(),
                c[9].exchange(),
                c[10].exchange(),
                c[11].exchange(),
                c[12].exchange(),
                c[13].exchange(),
                c[14].exchange(),
                c[15].exchange()
            );
        }
    }

    pub async fn finish(self) {
        let concurrency = self.connections.len();
        let requests: u64 = self
            .connections
            .iter()
            .map(|connection| connection.completed.load(Ordering::Relaxed))
            .sum();
        match resources() {
            Ok(snapshot) => {
                eprintln!(
                    "SPLICE_RESOURCE engine={} concurrency={concurrency} completed_roundtrips={requests} fds={} threads={:?}",
                    self.engine.name(),
                    snapshot.fds,
                    snapshot.threads
                );
                eprintln!(
                    "SPLICE_AFFINITY engine={} threads={:?}",
                    self.engine.name(),
                    snapshot.affinity
                );
            }
            Err(error) => eprintln!(
                "SPLICE_RESOURCE_UNAVAILABLE engine={} error={error}",
                self.engine.name()
            ),
        }
        for connection in self.connections {
            connection.finish().await;
        }
        #[cfg(feature = "io-uring")]
        if let Some(mut driver) = self.driver {
            let report = driver.shutdown().unwrap().unwrap();
            assert_eq!(report.accepted, concurrency as u64);
            assert_eq!(report.completed, concurrency as u64);
            assert_eq!(report.rejected, 0);
            assert_eq!(report.abandoned_receivers, 0);
            eprintln!("SPLICE_URING_REPORT {report:?}");
        }
    }
}

struct ResourceSnapshot {
    fds: usize,
    threads: Vec<String>,
    affinity: Vec<String>,
}

fn resources() -> io::Result<ResourceSnapshot> {
    let fds = std::fs::read_dir("/proc/self/fd")?
        .try_fold(0usize, |count, entry| entry.map(|_| count + 1))?;
    let mut threads = Vec::new();
    let mut affinity = Vec::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let path = entry?.path();
        let name = std::fs::read_to_string(path.join("comm"))?
            .trim()
            .to_owned();
        let status = std::fs::read_to_string(path.join("status"))?;
        let cpus = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .ok_or_else(|| io::Error::other("missing thread CPU affinity"))?
            .trim();
        affinity.push(format!("{name}:{cpus}"));
        threads.push(name);
    }
    threads.sort();
    affinity.sort();
    Ok(ResourceSnapshot {
        fds,
        threads,
        affinity,
    })
}
