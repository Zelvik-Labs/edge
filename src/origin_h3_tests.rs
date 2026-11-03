use super::*;
use pingora_core::upstreams::peer::HttpPeer;
use std::future::Future;
use std::sync::atomic::AtomicUsize;
use tokio::task::JoinSet;

pub(crate) fn completed_pool_contexts(
    count: usize,
    check: impl FnOnce(Vec<(OriginH3PoolKey, Arc<OriginH3Connection>)>),
) {
    wire_test(async {
        let origin = Origin::start();
        let mut contexts = Vec::new();
        let mut weak = Vec::new();
        for index in 0..count {
            let mut peer = origin.peer();
            peer.group_key = index as u64;
            let mut session = new_session(&peer).await;
            session
                .write_request_header(request("/pool", "GET"), true)
                .await
                .unwrap();
            read_complete(&mut session, "/pool", b"").await;
            weak.push(Arc::downgrade(&session.context));
            contexts.push((session.context.key.clone(), session.context.clone()));
        }
        check(contexts);
        assert!(weak.iter().all(|context| context.upgrade().is_none()));
        wait_count(&origin.stats.closed, count).await;
        origin.stop().await;
    });
}

/// Explicitly invoked by the paired CI harness. Setup and clean shutdown use
/// real Kernel-loopback H3; the timed region measures only production pool
/// take/offer, not HTTP throughput. The ordinary wire tests keep their 15s cap.
#[test]
#[ignore = "real H3 pool cost probe; run via scripts/perf/h3-pool/run.py"]
fn real_idle_pool_cost_probe() {
    let count: usize = std::env::var("EDGE_H3_POOL_KEYS").unwrap().parse().unwrap();
    let cycles: usize = std::env::var("EDGE_H3_POOL_CYCLES")
        .unwrap()
        .parse()
        .unwrap();
    assert!([1, 16, 256, 1024].contains(&count));
    assert!(cycles >= count);
    let output = std::path::PathBuf::from(std::env::var_os("EDGE_H3_POOL_RESULT").unwrap());
    crate::runtime_mode::with_runtime_config_for_test(
        crate::runtime_mode::RuntimeConfig::default(),
        || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(180), async {
                        let origin = Origin::start();
                        let slots = crate::origin_h3_pool::available_slots_for_test();
                        let mut identities = Vec::new();
                        for index in 0..count {
                            let mut peer = origin.peer();
                            peer.group_key = index as u64;
                            let mut session = new_session(&peer).await;
                            session
                                .write_request_header(request("/pool", "GET"), true)
                                .await
                                .unwrap();
                            read_complete(&mut session, "/pool", b"").await;
                            identities.push((
                                session.context.key.clone(),
                                session.context.quic.stable_id(),
                                session.context.driver.id(),
                                Arc::downgrade(&session.context),
                            ));
                            OriginH3Connector
                                .release_http_session(session, &peer, None)
                                .await;
                        }
                        assert_eq!(
                            crate::origin_h3_pool::available_slots_for_test(),
                            slots - count
                        );
                        let mut blocks = Vec::new();
                        for block in 0..20 {
                            let start = std::time::Instant::now();
                            for cycle in 0..cycles {
                                let key = &identities[(block * cycles + cycle) % count].0;
                                let context =
                                    crate::origin_h3_pool::take(std::hint::black_box(key))
                                        .expect("healthy idle context");
                                crate::origin_h3_pool::offer(key.clone(), context);
                            }
                            blocks.push(start.elapsed().as_nanos());
                            tokio::task::yield_now().await;
                        }
                        assert_eq!(
                            crate::origin_h3_pool::available_slots_for_test(),
                            slots - count
                        );
                        for (key, quic_id, driver_id, _) in &identities {
                            let context = crate::origin_h3_pool::take(key).unwrap();
                            assert_eq!(context.quic.stable_id(), *quic_id);
                            assert_eq!(context.driver.id(), *driver_id);
                            assert!(context.is_open());
                        }
                        assert!(
                            identities
                                .iter()
                                .all(|(_, _, _, weak)| weak.upgrade().is_none())
                        );
                        assert_eq!(crate::origin_h3_pool::available_slots_for_test(), slots);
                        wait_count(&origin.stats.closed, count).await;
                        assert_eq!(origin.stats.requests.load(Ordering::Acquire), count);
                        assert_eq!(origin.stats.accepted.load(Ordering::Acquire), count);
                        origin.stop().await;
                        std::fs::write(
                            output,
                            serde_json::to_vec_pretty(&serde_json::json!({
                                "passed": true, "keys": count, "cycles_per_block": cycles,
                                "block_ns": blocks, "closed_connections": count,
                                "allocator": "System (library test executable)",
                                "scope": "real idle H3 take+offer only; network setup excluded"
                            }))
                            .unwrap(),
                        )
                        .unwrap();
                    })
                    .await
                    .unwrap();
                });
        },
    );
}

#[test]
fn response_header_conversion_preserves_repeated_fields_and_filters_connection_fields() {
    let response = http::Response::builder()
        .status(200)
        .header("set-cookie", "a=1")
        .header("set-cookie", "b=2")
        .header("x-origin-value", "first")
        .header("x-origin-value", "second")
        .header("CoNnEcTiOn", "keep-alive")
        .header("keep-alive", "timeout=5")
        .body(())
        .unwrap();
    let header = build_response_header(response).unwrap();
    for (name, expected) in [
        ("set-cookie", ["a=1", "b=2"]),
        ("x-origin-value", ["first", "second"]),
    ] {
        let actual = header
            .headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
    assert!(!header.headers.contains_key("connection"));
    assert!(!header.headers.contains_key("keep-alive"));
}

fn wire_test(test: impl Future<Output = ()>) {
    wire_test_with_timeout(Duration::from_secs(15), test);
}

fn wire_test_with_timeout(timeout: Duration, test: impl Future<Output = ()>) {
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
                .block_on(async { tokio::time::timeout(timeout, test).await.unwrap() });
        },
    );
}

struct Stats {
    accepted: AtomicUsize,
    requests: AtomicUsize,
    closed: AtomicUsize,
    multiplex_gate: tokio::sync::Barrier,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            accepted: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            closed: AtomicUsize::new(0),
            multiplex_gate: tokio::sync::Barrier::new(32),
        }
    }
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
    let mut requests: JoinSet<Result<(), String>> = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            completed = requests.join_next(), if !requests.is_empty() => {
                completed.unwrap().map_err(|error| error.to_string())??;
                continue;
            }
            accepted = h3.accept() => accepted,
        };
        let resolver = match accepted {
            Ok(Some(resolver)) => resolver,
            Ok(None) => break,
            Err(_) if expected_close(&connection, &stopping) => break,
            Err(error) => return Err(format!("H3 accept: {error}")),
        };
        requests.spawn(serve_request(
            resolver,
            connection.clone(),
            stats.clone(),
            stopping.clone(),
        ));
    }
    while let Some(result) = requests.join_next().await {
        result.map_err(|error| error.to_string())??;
    }
    Ok(())
}

async fn serve_request(
    resolver: h3::server::RequestResolver<h3_quinn::Connection, Bytes>,
    connection: quinn::Connection,
    stats: Arc<Stats>,
    stopping: Arc<AtomicBool>,
) -> Result<(), String> {
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
        return Ok(());
    }
    if path == "/reset-response" {
        stream
            .send_response(http::Response::builder().status(200).body(()).unwrap())
            .await
            .map_err(|e| e.to_string())?;
        stream.stop_stream(Code::H3_INTERNAL_ERROR);
        return Ok(());
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
    if path == "/multiplex" {
        stats.multiplex_gate.wait().await;
    }
    let response = response_bytes(&path, &body);
    stream
        .send_response(
            http::Response::builder()
                .status(200)
                .header("content-length", response.len().to_string())
                .header("set-cookie", "session=abc; HttpOnly")
                .header("set-cookie", "theme=dark; Secure")
                .header("x-origin-value", "first")
                .header("x-origin-value", "second")
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
    Ok(())
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
        let headers = &session.response_header().unwrap().headers;
        let cookies = headers
            .get_all("set-cookie")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(cookies, ["session=abc; HttpOnly", "theme=dark; Secure"]);
        let repeated = headers
            .get_all("x-origin-value")
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(repeated, ["first", "second"]);
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

#[test]
fn thirty_two_active_requests_multiplex_one_origin_connection() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let slots = crate::origin_h3_active::slots_for_test();
        let first = new_session(&peer).await;
        let id = first.context.quic.stable_id();
        let driver = first.context.driver.id();
        let key = first.context.key.clone();
        let weak = Arc::downgrade(&first.context);
        let mut sessions = vec![first];
        for _ in 1..32 {
            let session = OriginH3Connector.reused_http_session(&peer).await.unwrap();
            assert_eq!(session.context.quic.stable_id(), id);
            assert_eq!(session.context.driver.id(), driver);
            sessions.push(session);
        }
        assert!(
            OriginH3Connector.reused_http_session(&peer).await.is_none(),
            "local stream budget is bounded"
        );
        let mut requests = JoinSet::new();
        for mut session in sessions {
            let peer = peer.clone();
            requests.spawn(async move {
                session
                    .write_request_header(request("/multiplex", "GET"), true)
                    .await
                    .unwrap();
                // Origin waits for all 32 requests before sending any response.
                read_complete(&mut session, "/multiplex", b"").await;
                OriginH3Connector
                    .release_http_session(session, &peer, None)
                    .await;
            });
        }
        while let Some(result) = requests.join_next().await {
            result.unwrap();
        }
        assert_eq!(origin.stats.accepted.load(Ordering::Acquire), 1);
        assert_eq!(origin.stats.requests.load(Ordering::Acquire), 32);
        assert_eq!(crate::origin_h3_active::slots_for_test(), slots - 1);
        drop(crate::origin_h3_pool::take(&key).unwrap());
        assert!(weak.upgrade().is_none());
        assert_eq!(crate::origin_h3_active::slots_for_test(), slots);
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}

#[test]
fn failed_stream_drains_new_reuse_without_cancelling_an_active_peer() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut partial = new_session(&peer).await;
        let old_id = partial.context.quic.stable_id();
        let key = partial.context.key.clone();
        let weak = Arc::downgrade(&partial.context);
        let mut other = OriginH3Connector.reused_http_session(&peer).await.unwrap();
        partial
            .write_request_header(request("/hold-response", "GET"), true)
            .await
            .unwrap();
        partial.read_response_header().await.unwrap();
        assert_eq!(
            partial.read_response_body().await.unwrap().unwrap(),
            Bytes::from_static(b"part")
        );
        OriginH3Connector
            .release_http_session(partial, &peer, None)
            .await;
        assert!(OriginH3Connector.reused_http_session(&peer).await.is_none());
        assert!(
            other.context.is_open(),
            "failed request must not close another active stream"
        );
        let replacement = new_session(&peer).await;
        assert_ne!(replacement.context.quic.stable_id(), old_id);
        other
            .write_request_header(request("/survivor", "GET"), true)
            .await
            .unwrap();
        other.read_response_header().await.unwrap();
        let mut body = Vec::new();
        while let Some(bytes) = other.read_response_body().await.unwrap() {
            body.extend_from_slice(&bytes);
        }
        assert_eq!(body.as_slice(), response_bytes("/survivor", b"").as_ref());
        assert_eq!(
            other.read_trailers().await.unwrap().unwrap()["x-body-bytes"],
            "0"
        );
        assert!(
            !other.can_reuse_connection(),
            "context stays draining after a failed sibling"
        );
        OriginH3Connector
            .release_http_session(other, &peer, None)
            .await;
        assert!(weak.upgrade().is_none());
        let borrowed = crate::origin_h3_active::get(&key).unwrap();
        assert_eq!(
            borrowed.quic.stable_id(),
            replacement.context.quic.stable_id(),
            "old drop cannot remove a newer registry entry"
        );
        drop(borrowed);
        drop(replacement);
        wait_count(&origin.stats.closed, 2).await;
        origin.stop().await;
    });
}

#[test]
fn idle_retention_expiry_leaves_an_active_request_alive() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut first = new_session(&peer).await;
        let key = first.context.key.clone();
        let id = first.context.quic.stable_id();
        let mut active = OriginH3Connector.reused_http_session(&peer).await.unwrap();
        first
            .write_request_header(request("/retention", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut first, "/retention", b"").await;
        OriginH3Connector
            .release_http_session(first, &peer, None)
            .await;
        crate::origin_h3_pool::expire_for_test(&key);
        crate::origin_h3_pool::sweep_idle();
        assert!(active.context.is_open());
        assert_eq!(active.context.quic.stable_id(), id);
        active
            .write_request_header(request("/after-expiry", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut active, "/after-expiry", b"").await;
        OriginH3Connector
            .release_http_session(active, &peer, None)
            .await;
        drop(crate::origin_h3_pool::take(&key).unwrap());
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}

#[test]
fn active_context_budget_saturation_opens_a_second_connection() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let first = new_session(&peer).await;
        let id = first.context.quic.stable_id();
        let mut sessions = vec![first];
        for _ in 1..32 {
            sessions.push(OriginH3Connector.reused_http_session(&peer).await.unwrap());
        }
        let second = new_session(&peer).await;
        assert_ne!(second.context.quic.stable_id(), id);
        drop(sessions);
        drop(second);
        wait_count(&origin.stats.closed, 2).await;
        origin.stop().await;
    });
}

/// Same executable, same origin policy and real HTTP exchanges. Disabling only
/// the active index reproduces exclusive idle-pool borrowing for this ablation;
/// it is not an exact historical binary or complete Edge benchmark.
#[test]
#[ignore = "real concurrent origin reuse ablation; explicitly run in CI"]
fn active_origin_reuse_cost_probe() {
    let enabled = match std::env::var("EDGE_H3_ACTIVE_REUSE").unwrap().as_str() {
        "on" => true,
        "off" => false,
        other => panic!("unknown probe mode {other}"),
    };
    let output = std::path::PathBuf::from(std::env::var_os("EDGE_H3_ACTIVE_RESULT").unwrap());
    wire_test_with_timeout(Duration::from_secs(180), async {
        crate::origin_h3_active::set_probe_reuse(enabled);
        let origin = Origin::start();
        let peer = origin.peer();
        let mut warm = new_session(&peer).await;
        let key = warm.context.key.clone();
        warm.write_request_header(request("/warm", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut warm, "/warm", b"").await;
        OriginH3Connector
            .release_http_session(warm, &peer, None)
            .await;
        let mut waves = Vec::new();
        for _ in 0..100 {
            let start = std::time::Instant::now();
            let mut requests = JoinSet::new();
            for _ in 0..32 {
                let peer = peer.clone();
                requests.spawn(async move {
                    let (connection, reused) =
                        OriginH3Connector.get_http_session(&peer).await.unwrap();
                    let Connection::Session(mut session) = connection else {
                        panic!("unexpected H1 fallback")
                    };
                    session
                        .write_request_header(request("/multiplex", "GET"), true)
                        .await
                        .unwrap();
                    read_complete(&mut session, "/multiplex", b"").await;
                    OriginH3Connector
                        .release_http_session(session, &peer, None)
                        .await;
                    reused
                });
            }
            let mut reused = 0;
            while let Some(result) = requests.join_next().await {
                reused += usize::from(result.unwrap());
            }
            assert_eq!(reused, if enabled { 32 } else { 1 });
            waves.push(start.elapsed().as_nanos());
        }
        drop(crate::origin_h3_pool::take(&key).unwrap());
        let accepted = origin.stats.accepted.load(Ordering::Acquire);
        assert_eq!(accepted, if enabled { 1 } else { 3101 });
        assert_eq!(origin.stats.requests.load(Ordering::Acquire), 3201);
        wait_count(&origin.stats.closed, accepted).await;
        origin.stop().await;
        crate::origin_h3_active::set_probe_reuse(true);
        std::fs::write(
            output,
            serde_json::to_vec_pretty(&serde_json::json!({
                "passed": true, "active_reuse": enabled, "concurrency": 32,
                "wave_ns": waves, "measured_requests": 3200, "warmup_requests": 1,
                "accepted_and_closed_connections": accepted,
                "scope": "real H3 origin connector, System allocator library test; not whole Node"
            }))
            .unwrap(),
        )
        .unwrap();
    });
}

#[test]
fn failed_active_request_discards_a_completed_siblings_idle_retention() {
    wire_test(async {
        let origin = Origin::start();
        let peer = origin.peer();
        let mut completed = new_session(&peer).await;
        let key = completed.context.key.clone();
        let weak = Arc::downgrade(&completed.context);
        let mut partial = OriginH3Connector.reused_http_session(&peer).await.unwrap();
        completed
            .write_request_header(request("/completed", "GET"), true)
            .await
            .unwrap();
        read_complete(&mut completed, "/completed", b"").await;
        OriginH3Connector
            .release_http_session(completed, &peer, None)
            .await;
        partial
            .write_request_header(request("/hold-response", "GET"), true)
            .await
            .unwrap();
        partial.read_response_header().await.unwrap();
        assert_eq!(
            partial.read_response_body().await.unwrap().unwrap(),
            Bytes::from_static(b"part")
        );
        OriginH3Connector
            .release_http_session(partial, &peer, None)
            .await;
        assert!(
            weak.upgrade().is_none(),
            "failed request removes idle retention immediately"
        );
        assert!(crate::origin_h3_pool::take(&key).is_none());
        wait_count(&origin.stats.closed, 1).await;
        origin.stop().await;
    });
}
