use super::*;
use pingora_core::upstreams::peer::HttpPeer;
use std::future::Future;
use std::sync::atomic::AtomicUsize;
use tokio::task::JoinSet;

fn wire_test(test: impl Future<Output = ()>) {
    // Own a controlled Kernel policy for the fixture and restore even a
    // previously uninitialized config on panic. The guard stays synchronous.
    crate::runtime_mode::with_runtime_config_for_test(
        crate::runtime_mode::RuntimeConfig::default(),
        || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(15), test)
                        .await
                        .unwrap()
                });
        },
    );
}

#[derive(Default)]
struct Stats {
    accepted: AtomicUsize,
    requests: AtomicUsize,
    closed: AtomicUsize,
}

struct Origin {
    endpoint: Endpoint,
    stats: Arc<Stats>,
    stopping: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

fn server_config() -> quinn::ServerConfig {
    let certs = rustls_pemfile::certs(
        &mut include_bytes!("../pingora-main/pingora-core/examples/keys/server/cert.pem")
            .as_slice(),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let key = rustls_pemfile::private_key(
        &mut include_bytes!("../pingora-main/pingora-core/examples/keys/server/key.pem").as_slice(),
    )
    .unwrap()
    .unwrap();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        crate::tls_crypto::default_crypto_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(Arc::new(tls)).unwrap(),
    ))
}

fn expected_close(connection: &quinn::Connection, stopping: &AtomicBool) -> bool {
    stopping.load(Ordering::Acquire)
        || matches!(
            connection.close_reason(),
            Some(quinn::ConnectionError::ApplicationClosed(reason))
                if reason.error_code == quinn::VarInt::from_u32(Code::H3_NO_ERROR.value() as u32)
        )
}

async fn serve(
    connection: quinn::Connection,
    stats: Arc<Stats>,
    stopping: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut h3: h3::server::Connection<h3_quinn::Connection, Bytes> = h3::server::builder()
        .build(h3_quinn::Connection::new(connection.clone()))
        .await
        .map_err(|e| e.to_string())?;
    loop {
        let resolver = match h3.accept().await {
            Ok(Some(resolver)) => resolver,
            Ok(None) => return Ok(()),
            Err(_) if expected_close(&connection, &stopping) => return Ok(()),
            Err(error) => return Err(format!("H3 accept: {error}")),
        };
        let (request, mut stream) = resolver
            .resolve_request()
            .await
            .map_err(|e| e.to_string())?;
        stats.requests.fetch_add(1, Ordering::AcqRel);
        let path = request.uri().path().to_owned();
        if path == "/hold-upload" {
            let reason = connection.closed().await;
            return if expected_close(&connection, &stopping) {
                Ok(())
            } else {
                Err(reason.to_string())
            };
        }
        // Headers deliberately wait for the entire upload. A client holding
        // the bidirectional stream mutex while awaiting headers deadlocks here.
        let mut body = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
            let len = chunk.remaining();
            body.extend_from_slice(&chunk.copy_to_bytes(len));
        }
        let request_trailers = stream.recv_trailers().await.map_err(|e| e.to_string())?;
        if request_trailers.is_some() {
            return Err("unexpected request trailers".to_owned());
        }
        if path == "/empty" {
            stream
                .send_response(http::Response::builder().status(204).body(()).unwrap())
                .await
                .map_err(|e| e.to_string())?;
            stream.finish().await.map_err(|e| e.to_string())?;
            continue;
        }
        if path == "/reset-response" {
            stream
                .send_response(http::Response::builder().status(200).body(()).unwrap())
                .await
                .map_err(|e| e.to_string())?;
            stream.stop_stream(Code::H3_INTERNAL_ERROR);
            continue;
        }
        if path == "/hold-response" {
            stream
                .send_response(
                    http::Response::builder()
                        .status(200)
                        .header("content-length", "99")
                        .body(())
                        .unwrap(),
                )
                .await
                .map_err(|e| e.to_string())?;
            stream
                .send_data(Bytes::from_static(b"part"))
                .await
                .map_err(|e| e.to_string())?;
            let reason = connection.closed().await;
            return if expected_close(&connection, &stopping) {
                Ok(())
            } else {
                Err(reason.to_string())
            };
        }
        let response = response_bytes(&path, &body);
        stream
            .send_response(
                http::Response::builder()
                    .status(200)
                    .header("content-length", response.len().to_string())
                    .body(())
                    .unwrap(),
            )
            .await
            .map_err(|e| e.to_string())?;
        stream
            .send_data(response)
            .await
            .map_err(|e| e.to_string())?;
        let mut trailers = HeaderMap::new();
        trailers.insert("x-body-bytes", body.len().to_string().parse().unwrap());
        stream
            .send_trailers(trailers)
            .await
            .map_err(|e| e.to_string())?;
        if path == "/hold-trailers" {
            let reason = connection.closed().await;
            return if expected_close(&connection, &stopping) {
                Ok(())
            } else {
                Err(reason.to_string())
            };
        }
        stream.finish().await.map_err(|e| e.to_string())?;
    }
}

impl Origin {
    fn start() -> Self {
        let endpoint = Endpoint::server(server_config(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let listener = endpoint.clone();
        let stats = Arc::new(Stats::default());
        let counters = stats.clone();
        let stopping = Arc::new(AtomicBool::new(false));
        let shutdown = stopping.clone();
        let task = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            while let Some(incoming) = listener.accept().await {
                let counters = counters.clone();
                let shutdown = shutdown.clone();
                handlers.spawn(async move {
                    let connection = incoming.await.map_err(|e| e.to_string())?;
                    counters.accepted.fetch_add(1, Ordering::AcqRel);
                    let result = serve(connection, counters.clone(), shutdown).await;
                    counters.closed.fetch_add(1, Ordering::AcqRel);
                    result
                });
            }
            while let Some(result) = handlers.join_next().await {
                result.map_err(|e| e.to_string())??;
            }
            Ok(())
        });
        Self {
            endpoint,
            stats,
            stopping,
            task,
        }
    }

    fn peer(&self) -> HttpPeer {
        let mut peer = HttpPeer::new(
            self.endpoint.local_addr().unwrap(),
            true,
            "localhost".to_owned(),
        );
        peer.options.verify_cert = false;
        peer.options.verify_hostname = false;
        peer.options.connection_timeout = Some(Duration::from_secs(3));
        peer.options.read_timeout = Some(Duration::from_secs(3));
        peer.options.write_timeout = Some(Duration::from_secs(3));
        peer
    }

    async fn stop(mut self) {
        self.stopping.store(true, Ordering::Release);
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"test complete");
        tokio::time::timeout(Duration::from_secs(3), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            self.stats.closed.load(Ordering::Acquire),
            self.stats.accepted.load(Ordering::Acquire)
        );
        if let Some(output) = std::env::var_os("EDGE_H3_TEST_OUTPUT") {
            let thread = std::thread::current();
            let name = thread.name().expect("named wire test thread");
            let filename: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let directory = std::path::PathBuf::from(output);
            std::fs::create_dir_all(&directory).unwrap();
            let report = serde_json::json!({
                "passed": true,
                "test": name,
                "accepted_quic_connections": self.stats.accepted.load(Ordering::Acquire),
                "resolved_h3_requests": self.stats.requests.load(Ordering::Acquire),
                "closed_quic_connections": self.stats.closed.load(Ordering::Acquire),
                "scope": "real Kernel-loopback H3 origin, not throughput or real-NIC AF_XDP",
            });
            std::fs::write(
                directory.join(format!("{filename}.json")),
                serde_json::to_vec_pretty(&report).unwrap(),
            )
            .unwrap();
        }
    }
}

impl Drop for Origin {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.endpoint
            .close(quinn::VarInt::from_u32(0), b"test teardown");
        self.task.abort();
    }
}

fn response_bytes(path: &str, body: &[u8]) -> Bytes {
    let mut bytes = format!("wire:{path}:").into_bytes();
    bytes.extend_from_slice(body);
    Bytes::from(bytes)
}

fn request(path: &str, method: &str) -> Box<RequestHeader> {
    let mut req = RequestHeader::build(method, path.as_bytes(), Some(2)).unwrap();
    req.insert_header("host", "localhost").unwrap();
    Box::new(req)
}

async fn new_session(peer: &HttpPeer) -> OriginH3ClientSession {
    let (connection, reused) = OriginH3Connector.get_http_session(peer).await.unwrap();
    assert!(!reused);
    match connection {
        Connection::Session(session) => session,
        Connection::Stream(_) => panic!("unexpected H1 fallback"),
    }
}

async fn read_complete(session: &mut OriginH3ClientSession, path: &str, body: &[u8]) {
    session.read_response_header().await.unwrap();
    session.check_response_end_or_error(true).await.unwrap();
    let mut response = Vec::new();
    while let Some(chunk) = session.read_response_body().await.unwrap() {
        response.extend_from_slice(&chunk);
    }
    let trailers = session.read_trailers().await.unwrap();
    if path == "/empty" {
        assert!(response.is_empty());
        assert!(trailers.is_none());
    } else {
        assert_eq!(response.as_slice(), response_bytes(path, body).as_ref());
        assert_eq!(
            trailers.unwrap()["x-body-bytes"].to_str().unwrap(),
            body.len().to_string()
        );
    }
    assert!(session.can_reuse_connection());
}

async fn wait_count(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while counter.load(Ordering::Acquire) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counter.load(Ordering::Acquire), expected);
}

#[test]
fn sequential_h3_requests_reuse_one_protocol_driver_and_quic_connection() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        crate::origin_h3_pool::sweep_idle();
        let slots = crate::origin_h3_pool::available_slots_for_test();
        let mut session = new_session(&peer).await;
        let id = session.context.quic.stable_id();
        let driver = session.context.driver.id();
        let key = session.context.key.clone();
        let weak = Arc::downgrade(&session.context);
        for index in 0..12 {
            let path = if index == 6 {
                "/empty".to_owned()
            } else {
                format!("/r{index}")
            };
            session
                .write_request_header(request(&path, "GET"), true)
                .await
                .unwrap();
            read_complete(&mut session, &path, b"").await;
            OriginH3Connector
                .release_http_session(session, &peer, None)
                .await;
            assert_eq!(crate::origin_h3_pool::available_slots_for_test(), slots - 1);
            session = OriginH3Connector
                .reused_http_session(&peer)
                .await
                .expect("completed H3 session must be reusable");
            assert_eq!(session.context.quic.stable_id(), id);
            assert_eq!(session.context.driver.id(), driver);
            assert_eq!(crate::origin_h3_pool::available_slots_for_test(), slots);
        }
        assert_eq!(origin.stats.accepted.load(Ordering::Acquire), 1);
        assert_eq!(origin.stats.requests.load(Ordering::Acquire), 12);
        assert!(crate::origin_h3_pool::take(&key).is_none());
        drop(session);
        assert!(weak.upgrade().is_none());
        wait_count(&origin.stats.closed, 1).await;
        assert_eq!(crate::origin_h3_pool::available_slots_for_test(), slots);
        origin.stop().await;
    });
}

#[test]
fn request_upload_progresses_while_response_headers_are_pending() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut session = new_session(&peer).await;
        let key = session.context.key.clone();
        session
            .write_request_header(request("/post", "POST"), false)
            .await
            .unwrap();
        let mut writer = session.take_request_body_writer().unwrap();
        let body = vec![0x53; 128 * 1024];
        let mut send = Bytes::copy_from_slice(&body);
        let (header, write) = tokio::join!(session.read_response_header(), async {
            writer.write_all_buf(&mut send).await?;
            writer.finish().await?;
            writer.finish().await
        });
        header.unwrap();
        write.unwrap();
        writer.cleanup().await.unwrap();
        drop(writer);
        session.finish_request_body().await.unwrap();
        let mut received = Vec::new();
        while let Some(chunk) = session.read_response_body().await.unwrap() {
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received.as_slice(), response_bytes("/post", &body).as_ref());
        assert_eq!(
            session.read_trailers().await.unwrap().unwrap()["x-body-bytes"]
                .to_str()
                .unwrap(),
            body.len().to_string()
        );
        assert!(session.can_reuse_connection());
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        drop(crate::origin_h3_pool::take(&key).unwrap());
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}

#[test]
fn partial_response_and_reset_are_not_returned_to_the_idle_pool() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut session = new_session(&peer).await;
        let key = session.context.key.clone();
        session
            .write_request_header(request("/hold-response", "GET"), true)
            .await
            .unwrap();
        session.read_response_header().await.unwrap();
        assert_eq!(
            session.read_response_body().await.unwrap().unwrap(),
            Bytes::from_static(b"part")
        );
        assert!(!session.can_reuse_connection());
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 1).await;
        let mut session = new_session(&peer).await;
        session
            .write_request_header(request("/reset-response", "GET"), true)
            .await
            .unwrap();
        let failed = if session.read_response_header().await.is_err() {
            true
        } else {
            loop {
                match session.read_response_body().await {
                    Err(_) => break true,
                    Ok(None) => break false,
                    Ok(Some(_)) => {}
                }
            }
        };
        assert!(failed, "wire RESET must surface as an error");
        assert!(!session.can_reuse_connection());
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 2).await;
        origin.stop().await;
    });
}

#[test]
fn abandoned_request_body_writer_cannot_be_finished_into_a_healthy_pool_entry() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        for (index, cleanup) in [false, true].into_iter().enumerate() {
            let mut session = new_session(&peer).await;
            let key = session.context.key.clone();
            session
                .write_request_header(request("/hold-upload", "POST"), false)
                .await
                .unwrap();
            let mut writer = session.take_request_body_writer().unwrap();
            let mut data = Bytes::from_static(b"partial");
            writer.write_all_buf(&mut data).await.unwrap();
            wait_count(&origin.stats.requests, index + 1).await;
            if cleanup {
                writer.cleanup().await.unwrap();
            }
            drop(writer);
            assert!(session.finish_request_body().await.is_err());
            assert!(!session.can_reuse_connection());
            OriginH3Connector
                .release_http_session(session, &peer, None)
                .await;
            assert!(crate::origin_h3_pool::take(&key).is_none());
            wait_count(&origin.stats.closed, index + 1).await;
        }
        origin.stop().await;
    });
}

#[test]
fn pending_header_and_trailer_timeouts_do_not_prove_reusable_completion() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut session = new_session(&peer).await;
        let key = session.context.key.clone();
        session
            .write_request_header(request("/hold-upload", "GET"), true)
            .await
            .unwrap();
        wait_count(&origin.stats.requests, 1).await;
        session.set_read_timeout(Some(Duration::from_millis(20)));
        assert_eq!(
            session.read_response_header().await.unwrap_err().etype(),
            &ErrorType::ReadTimedout
        );
        assert!(!session.can_reuse_connection());
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 1).await;

        let mut session = new_session(&peer).await;
        session
            .write_request_header(request("/hold-trailers", "GET"), true)
            .await
            .unwrap();
        session.read_response_header().await.unwrap();
        let mut body = Vec::new();
        while let Some(chunk) = session.read_response_body().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(
            body.as_slice(),
            response_bytes("/hold-trailers", b"").as_ref()
        );
        assert!(!session.can_reuse_connection());
        session.set_read_timeout(Some(Duration::from_millis(20)));
        assert_eq!(
            session.read_trailers().await.unwrap_err().etype(),
            &ErrorType::ReadTimedout
        );
        assert!(!session.can_reuse_connection());
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 2).await;
        origin.stop().await;
    });
}

#[test]
fn pooled_h3_contexts_keep_peer_trust_and_group_isolation() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut session = new_session(&peer).await;
        let key = session.context.key.clone();
        session
            .write_request_header(request("/policy", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut session, "/policy", b"").await;
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        let mut strict = peer.clone();
        strict.options.verify_cert = true;
        strict.options.verify_hostname = true;
        assert!(
            OriginH3Connector
                .reused_http_session(&strict)
                .await
                .is_none()
        );
        let mut other_group = peer.clone();
        other_group.group_key += 1;
        assert!(
            OriginH3Connector
                .reused_http_session(&other_group)
                .await
                .is_none()
        );
        let session = OriginH3Connector
            .reused_http_session(&peer)
            .await
            .expect("original peer retains its own context");
        assert_eq!(session.context.key, key);
        drop(session);
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}

#[test]
fn idle_expiration_releases_the_sender_driver_and_endpoint() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut session = new_session(&peer).await;
        let key = session.context.key.clone();
        let weak = Arc::downgrade(&session.context);
        session
            .write_request_header(request("/expiry", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut session, "/expiry", b"").await;
        OriginH3Connector
            .release_http_session(session, &peer, None)
            .await;
        crate::origin_h3_pool::expire_for_test(&key);
        crate::origin_h3_pool::sweep_idle();
        assert!(weak.upgrade().is_none());
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}
