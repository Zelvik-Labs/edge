use bytes::{Buf, Bytes};
use futures_util::{StreamExt, future::poll_fn};
use h3::quic::{Connection as _, RecvStream as _, SendStream as _};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::task::JoinSet;
use tokio_quiche::{
    ConnectionParams,
    metrics::DefaultMetrics,
    settings::{CertificateKind, Hooks, QuicSettings, TlsCertificatePaths},
};
use zelvik_edge_h3_quiche::tls::HandshakeGate;
use zelvik_edge_h3_quiche::{CloseHandle, Connection, Limits};

struct AdmissionTls(HandshakeGate);
impl tokio_quiche::quic::ConnectionHook for AdmissionTls {
    fn create_custom_ssl_context_builder(
        &self,
        paths: TlsCertificatePaths<'_>,
    ) -> Option<boring::ssl::SslContextBuilder> {
        let mut builder =
            boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls()).unwrap();
        builder.set_certificate_chain_file(paths.cert).unwrap();
        builder
            .set_private_key_file(paths.private_key, boring::ssl::SslFiletype::PEM)
            .unwrap();
        builder.check_private_key().unwrap();
        let gate = self.0;
        builder.set_select_certificate_callback(move |hello| gate.check(hello.ssl()));
        Some(builder)
    }
}

static NEXT: AtomicUsize = AtomicUsize::new(0);
struct LossRelay {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
    dropped: Arc<[AtomicUsize; 2]>,
}
impl LossRelay {
    async fn new(server: std::net::SocketAddr, client: std::net::SocketAddr) -> Self {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let dropped = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let counts = dropped.clone();
        let task = tokio::spawn(async move {
            let mut packets = [0usize; 2];
            let mut buffer = vec![0; 65536];
            loop {
                let (size, peer) = socket.recv_from(&mut buffer).await.unwrap();
                let (direction, destination) = if peer == server {
                    (0, client)
                } else {
                    assert_eq!(peer, client);
                    (1, server)
                };
                packets[direction] += 1;
                // Deterministic ~2.7% loss in each direction, including ACKs.
                // This relay is a correctness fixture, never a throughput probe.
                if packets[direction].is_multiple_of(37) {
                    counts[direction].fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                assert_eq!(
                    socket.send_to(&buffer[..size], destination).await.unwrap(),
                    size
                );
            }
        });
        Self {
            address,
            task,
            dropped,
        }
    }
}
impl Drop for LossRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Fixture {
    directory: PathBuf,
    endpoint: quinn::Endpoint,
    client: quinn::Connection,
    server: Connection,
    close: CloseHandle,
    listener: tokio_quiche::QuicConnectionStream<DefaultMetrics>,
    gate: HandshakeGate,
    relay: Option<LossRelay>,
}
impl Fixture {
    async fn new(limits: Limits) -> Self {
        Self::with_loss(limits, false).await
    }
    async fn with_loss(limits: Limits, loss: bool) -> Self {
        let base = std::env::var_os("EDGE_QUICHE_EVIDENCE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&base).unwrap();
        let directory = base.join(format!(
            "edge-quiche-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&directory).unwrap();
        let cert = directory.join("cert.pem");
        let key = directory.join("key.pem");
        let output = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = socket.local_addr().unwrap();
        let mut settings = QuicSettings::default();
        settings.enable_dgram = false;
        settings.initial_max_streams_bidi = 32;
        settings.initial_max_streams_uni = 8;
        settings.initial_max_stream_data_bidi_remote = 64 * 1024;
        settings.initial_max_stream_data_bidi_local = 64 * 1024;
        settings.initial_max_stream_data_uni = 64 * 1024;
        settings.initial_max_data = 256 * 1024;
        settings.max_idle_timeout = Some(Duration::from_secs(5));
        if std::env::var_os("EDGE_QUICHE_EVIDENCE_DIR").is_some() {
            let qlog = directory.join("qlog");
            std::fs::create_dir(&qlog).unwrap();
            settings.qlog_dir = Some(qlog.to_str().unwrap().to_string());
        }
        let gate = HandshakeGate::new().unwrap();
        let hooks = Hooks {
            connection_hook: Some(Arc::new(AdmissionTls(gate))),
        };
        let params = ConnectionParams::new_server(
            settings,
            TlsCertificatePaths {
                cert: cert.to_str().unwrap(),
                private_key: key.to_str().unwrap(),
                kind: CertificateKind::X509,
            },
            hooks,
        );
        let mut listeners = tokio_quiche::listen([socket], params, DefaultMetrics).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for der in rustls_pemfile::certs(&mut std::io::Cursor::new(std::fs::read(&cert).unwrap())) {
            roots.add(der.unwrap()).unwrap();
        }
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap(),
        )));
        let relay = if loss {
            Some(LossRelay::new(address, endpoint.local_addr().unwrap()).await)
        } else {
            None
        };
        let connect = endpoint
            .connect(
                relay.as_ref().map_or(address, |relay| relay.address),
                "localhost",
            )
            .unwrap();
        let (driver, server, close) = zelvik_edge_h3_quiche::server(limits).unwrap();
        let mut listener = listeners.pop().unwrap();
        let incoming = listener.next().await.unwrap().unwrap();
        let _metadata = incoming.start(driver);
        let client = connect.await.unwrap();
        Self {
            directory,
            endpoint,
            client,
            server,
            close,
            listener,
            gate,
            relay,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_initials_finish_cleanup_without_admitting_application_streams() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut fixture = Fixture::new(Limits::default()).await;
        for _ in 0..16 {
            let connect = fixture
                .endpoint
                .connect(fixture.client.remote_address(), "localhost")
                .unwrap();
            let incoming = fixture.listener.next().await.unwrap().unwrap();
            let reason = zelvik_edge_h3_quiche::admission::reject(incoming, fixture.gate)
                .await
                .unwrap();
            assert!(!reason.is_empty());
            assert!(connect.await.is_err(), "rejected TLS must never establish");
        }
        let (mut tx, _rx) = fixture.client.open_bi().await.unwrap();
        tx.write_all(b"existing connection remains healthy")
            .await
            .unwrap();
        tx.finish().unwrap();
        let mut stream = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = poll_fn(|cx| stream.poll_data(cx)).await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, b"existing connection remains healthy");
        poll_fn(|cx| stream.poll_finish(cx)).await.unwrap();
    })
    .await
    .expect("admission rejection must run bounded cleanup");
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.close.close(0x100, b"test complete");
        self.endpoint.close(0_u32.into(), b"test complete");
        if std::env::var_os("EDGE_QUICHE_EVIDENCE_DIR").is_none() {
            std::fs::remove_dir_all(&self.directory).unwrap();
        } else {
            // Keep wire diagnostics but never upload generated private keys.
            std::fs::remove_file(self.directory.join("key.pem")).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_h3_multiplex_preserves_body_duplicate_headers_and_both_trailers() {
    multiplex_roundtrip(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_h3_multiplex_recovers_bidirectional_packet_loss() {
    multiplex_roundtrip(true).await;
}

async fn multiplex_roundtrip(loss: bool) {
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut fixture = Fixture::with_loss(Limits::default(), loss).await;
        // Move the H3-facing connection while retaining fixture cleanup ownership.
        let (_, replacement, _) = zelvik_edge_h3_quiche::server(Limits::default()).unwrap();
        let transport = std::mem::replace(&mut fixture.server, replacement);
        let mut server = h3::server::builder().build(transport).await.unwrap();
        let (mut driver, mut sender) =
            h3::client::new(h3_quinn::Connection::new(fixture.client.clone()))
                .await
                .unwrap();
        let client_driver = tokio::spawn(async move { poll_fn(|cx| driver.poll_close(cx)).await });
        let server_task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            for _ in 0..16 {
                let resolver = server.accept().await.unwrap().unwrap();
                tasks.spawn(async move {
                    let (request, mut stream) = resolver.resolve_request().await.unwrap();
                    assert_eq!(request.uri().path(), "/payload");
                    let mut body = Vec::new();
                    while let Some(mut bytes) = stream.recv_data().await.unwrap() {
                        body.extend_from_slice(&bytes.copy_to_bytes(bytes.remaining()));
                    }
                    assert_eq!(body, vec![0x5a; 128 * 1024]);
                    assert_eq!(
                        stream.recv_trailers().await.unwrap().unwrap()["x-request-end"],
                        "complete"
                    );
                    let mut response = http::Response::builder().status(200).body(()).unwrap();
                    response
                        .headers_mut()
                        .append("set-cookie", "a=1".parse().unwrap());
                    response
                        .headers_mut()
                        .append("set-cookie", "b=2".parse().unwrap());
                    stream.send_response(response).await.unwrap();
                    stream.send_data(Bytes::from(body)).await.unwrap();
                    let mut trailers = http::HeaderMap::new();
                    trailers.insert("x-response-end", "complete".parse().unwrap());
                    stream.send_trailers(trailers).await.unwrap();
                    stream.finish().await.unwrap();
                });
            }
            while let Some(result) = tasks.join_next().await {
                result.unwrap();
            }
            // Keep the H3 control driver alive until the client consumes all data.
            server
        });
        let mut requests = JoinSet::new();
        for _ in 0..16 {
            let mut stream = sender
                .send_request(
                    http::Request::post("https://localhost/payload")
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            requests.spawn(async move {
                stream
                    .send_data(Bytes::from(vec![0x5a; 128 * 1024]))
                    .await
                    .unwrap();
                let mut trailers = http::HeaderMap::new();
                trailers.insert("x-request-end", "complete".parse().unwrap());
                stream.send_trailers(trailers).await.unwrap();
                stream.finish().await.unwrap();
                let response = stream.recv_response().await.unwrap();
                assert_eq!(response.status(), 200);
                assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
                let mut body = Vec::new();
                while let Some(mut bytes) = stream.recv_data().await.unwrap() {
                    body.extend_from_slice(&bytes.copy_to_bytes(bytes.remaining()));
                }
                assert_eq!(body, vec![0x5a; 128 * 1024]);
                assert_eq!(
                    stream.recv_trailers().await.unwrap().unwrap()["x-response-end"],
                    "complete"
                );
            });
        }
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
        let server = server_task.await.unwrap();
        drop(server);
        drop(sender);
        client_driver.abort();
        assert!(client_driver.await.unwrap_err().is_cancelled());
        if let Some(relay) = &fixture.relay {
            let dropped = relay
                .dropped
                .each_ref()
                .map(|count| count.load(Ordering::Relaxed));
            eprintln!("loss fixture dropped server/client datagrams: {dropped:?}");
            assert!(dropped.iter().all(|count| *count > 0));
            assert!(!relay.task.is_finished(), "loss relay must remain healthy");
        }
    })
    .await
    .expect("real H3 multiplex timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fin_sent_after_all_body_bytes_are_acknowledged_still_reaches_peer() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut fixture = Fixture::new(Limits::default()).await;
        let (mut request, mut response) = fixture.client.open_bi().await.unwrap();
        request.write_all(b"request").await.unwrap();
        request.finish().unwrap();
        let mut stream = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        while poll_fn(|cx| stream.poll_data(cx)).await.unwrap().is_some() {}
        stream.send_data(Bytes::from_static(b"response")).unwrap();
        poll_fn(|cx| stream.poll_ready(cx)).await.unwrap();
        let mut body = [0; 8];
        response.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, b"response");
        // Budgeted send buffers are released on ACK: this forces the late-FIN
        // ordering behind upstream quiche #2665 instead of relying on a race.
        while fixture.close.buffered_send_bytes() != 0 {
            tokio::task::yield_now().await;
        }
        poll_fn(|cx| stream.poll_finish(cx)).await.unwrap();
        assert!(response.read_to_end(0).await.unwrap().is_empty());
    })
    .await
    .expect("FIN after the last DATA ACK must reach the peer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_budget_follows_bytes_clones_and_resumes_without_a_new_packet() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut fixture = Fixture::new(Limits {
            streams: 16,
            receive_bytes: 32 * 1024,
            send_bytes: 32 * 1024,
        })
        .await;
        let (mut tx, _rx) = fixture.client.open_bi().await.unwrap();
        let send = tokio::spawn(async move {
            tx.write_all(&vec![0x6b; 128 * 1024]).await.unwrap();
            tx.finish().unwrap();
        });
        let mut stream = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        let first = poll_fn(|cx| stream.poll_data(cx)).await.unwrap().unwrap();
        let copy = first.clone();
        let second = poll_fn(|cx| stream.poll_data(cx)).await.unwrap().unwrap();
        assert_eq!(fixture.close.buffered_receive_bytes(), 32 * 1024);
        drop(first);
        assert_eq!(
            fixture.close.buffered_receive_bytes(),
            32 * 1024,
            "clone must retain its allocation budget"
        );
        assert!(futures_util::poll!(poll_fn(|cx| stream.poll_data(cx))).is_pending());
        let mut total = copy.len() + second.len();
        drop(copy);
        drop(second);
        while let Some(bytes) = poll_fn(|cx| stream.poll_data(cx)).await.unwrap() {
            assert!(bytes.iter().all(|b| *b == 0x6b));
            total += bytes.len();
        }
        assert_eq!(total, 128 * 1024);
        send.await.unwrap();
        assert_eq!(fixture.close.buffered_receive_bytes(), 0);
        poll_fn(|cx| stream.poll_finish(cx)).await.unwrap();
    })
    .await
    .expect("receive backpressure failed to resume");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_reset_is_an_error_and_another_stream_can_finish() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut fixture = Fixture::new(Limits::default()).await;
        let (mut broken, _broken_rx) = fixture.client.open_bi().await.unwrap();
        broken.write_all(b"partial").await.unwrap();
        let mut incoming = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        let first = poll_fn(|cx| incoming.poll_data(cx)).await.unwrap().unwrap();
        assert_eq!(&first[..], b"partial");
        drop(first);
        broken.reset(0x10c_u32.into()).unwrap();
        assert!(matches!(
            poll_fn(|cx| incoming.poll_data(cx)).await,
            Err(h3::quic::StreamErrorIncoming::StreamTerminated { error_code: 0x10c })
        ));
        let (mut healthy, _healthy_rx) = fixture.client.open_bi().await.unwrap();
        healthy.write_all(b"complete").await.unwrap();
        healthy.finish().unwrap();
        let mut next = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        let mut body = Vec::new();
        while let Some(data) = poll_fn(|cx| next.poll_data(cx)).await.unwrap() {
            body.extend_from_slice(&data);
        }
        assert_eq!(body, b"complete");
        poll_fn(|cx| next.poll_finish(cx)).await.unwrap();
    })
    .await
    .expect("reset must not stall an unrelated stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fin_progresses_while_every_receive_allocation_is_still_owned() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut fixture = Fixture::new(Limits {
            streams: 16,
            receive_bytes: 32 * 1024,
            send_bytes: 32 * 1024,
        })
        .await;
        let (mut tx, _rx) = fixture.client.open_bi().await.unwrap();
        tx.write_all(&[0x72]).await.unwrap();
        let mut stream = poll_fn(|cx| fixture.server.poll_accept_bidi(cx))
            .await
            .unwrap();
        let mut held = Vec::new();
        for index in 0..2 {
            if index != 0 {
                tx.write_all(&[0x72]).await.unwrap();
            }
            let bytes = poll_fn(|cx| stream.poll_data(cx)).await.unwrap().unwrap();
            assert_eq!(&bytes[..], &[0x72]);
            held.push(bytes);
        }
        assert_eq!(fixture.close.buffered_receive_bytes(), 32 * 1024);
        // Deliberately send FIN only after the peer has used every receive
        // permit. Returning None must not depend on dropping these Bytes.
        tx.finish().unwrap();
        assert!(poll_fn(|cx| stream.poll_data(cx)).await.unwrap().is_none());
        assert!(
            held.iter()
                .all(|bytes| bytes.iter().all(|value| *value == 0x72))
        );
        assert_eq!(fixture.close.buffered_receive_bytes(), 32 * 1024);
        drop(held);
        assert_eq!(fixture.close.buffered_receive_bytes(), 0);
        poll_fn(|cx| stream.poll_finish(cx)).await.unwrap();
    })
    .await
    .expect("FIN must bypass a fully occupied payload budget");
}
