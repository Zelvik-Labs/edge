use std::fmt::Debug;
use std::net::{IpAddr, Ipv4Addr, SocketAddr as StdSocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures_util::Stream;
use h3::error::Code;
use http::{HeaderMap, Response, Version};
use pingora_core::connectors::http::custom::{Connection, Connector as CustomConnector};
use pingora_core::protocols::http::custom::client::Session as CustomClientSession;
use pingora_core::protocols::http::custom::{BodyWrite, CustomMessageWrite};
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_core::protocols::{Digest, UniqueIDType};
use pingora_core::upstreams::peer::Peer;
use pingora_core::{Error, ErrorType, Result as PingoraResult};
use pingora_http::{RequestHeader, ResponseHeader};
use quinn::{ClientConfig, Endpoint};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::sync::Mutex;

use crate::origin_h3_state::{ORIGIN_H3_STATE_MANAGER, OriginH3Key};

type H3SendRequest = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type H3RequestStream = h3::client::RequestStream<h3_quinn::SendStream<Bytes>, Bytes>;
type H3ResponseStream = h3::client::RequestStream<h3_quinn::RecvStream, Bytes>;

/// Reuse is scoped to the peer's full policy and the concrete upstream path,
/// not just authority. An AF socket retains its registry, so its pointer cannot
/// be recycled while a pooled context with that identity is alive.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OriginH3PoolKey {
    target: OriginH3Key,
    peer_reuse_hash: u64,
    verify_cert: bool,
    verify_hostname: bool,
    af_registry_identity: Option<usize>,
    transport: Option<(u8, bool, bool)>,
}

struct H3ConnectionPlan {
    key: OriginH3PoolKey,
    server_addr: StdSocketAddr,
    transport: Option<crate::runtime_mode::XdpTransportSettings>,
    #[cfg(target_os = "linux")]
    registry: Option<Arc<crate::xdp::af_xdp::AfXdpDialRegistry>>,
}

fn connection_plan<P: Peer>(peer: &P) -> PingoraResult<H3ConnectionPlan> {
    let server_addr = peer.address().as_inet().copied().ok_or_else(|| {
        Error::explain(
            ErrorType::ConnectError,
            "HTTP/3 origin requires an IP socket address",
        )
    })?;
    #[cfg(target_os = "linux")]
    let (transport, registry, af_registry_identity) = {
        let runtime = crate::runtime_mode::RuntimeConfig::current();
        let selected = runtime.as_ref().is_some_and(|runtime| {
            runtime.xdp.upstream_mode() == crate::runtime_mode::XdpUpstreamMode::Afxdp
        });
        let transport = selected.then(|| {
            runtime
                .and_then(|runtime| runtime.xdp.transport)
                .unwrap_or_default()
                .resolved()
        });
        let requested = selected && !server_addr.ip().is_loopback();
        let registry = if requested {
            crate::xdp::af_xdp_dial_registry()
        } else {
            None
        };
        let identity = requested.then(|| {
            registry
                .as_ref()
                .map_or(0, |registry| Arc::as_ptr(registry) as usize)
        });
        (transport, registry, identity)
    };
    #[cfg(not(target_os = "linux"))]
    let (transport, af_registry_identity): (
        Option<crate::runtime_mode::XdpTransportSettings>,
        Option<usize>,
    ) = (None, None);
    let key = OriginH3PoolKey {
        target: origin_h3_key(peer),
        peer_reuse_hash: peer.reuse_hash(),
        verify_cert: peer.verify_cert(),
        verify_hostname: peer.verify_hostname(),
        af_registry_identity,
        transport: transport.as_ref().map(|settings| {
            (
                settings.controller as u8,
                settings.trusted_ecn,
                settings.aggregation,
            )
        }),
    };
    Ok(H3ConnectionPlan {
        key,
        server_addr,
        transport,
        #[cfg(target_os = "linux")]
        registry,
    })
}

/// One H3 protocol context for the lifetime of one QUIC connection. Retaining
/// only QUIC handles drops the final h3 SendRequest and closes that connection.
pub struct OriginH3Connection {
    key: OriginH3PoolKey,
    _endpoint: Endpoint,
    quic: quinn::Connection,
    sender: Mutex<H3SendRequest>,
    driver: tokio::task::JoinHandle<()>,
    server_addr: StdSocketAddr,
    client_addr: StdSocketAddr,
}

impl OriginH3Connection {
    pub(crate) fn is_open(&self) -> bool {
        self.quic.close_reason().is_none() && !self.driver.is_finished()
    }
}

impl Drop for OriginH3Connection {
    fn drop(&mut self) {
        self.quic.close(
            quinn::VarInt::from_u32(Code::H3_NO_ERROR.value() as u32),
            b"origin H3 context released",
        );
        self.driver.abort();
    }
}

struct H3RequestSend {
    stream: Mutex<H3RequestStream>,
    finished: AtomicBool,
    failed: AtomicBool,
}

#[derive(Clone, Default)]
pub struct OriginH3Connector;

#[async_trait]
impl CustomConnector for OriginH3Connector {
    type Session = OriginH3ClientSession;

    async fn get_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> PingoraResult<(Connection<Self::Session>, bool)> {
        let key = origin_h3_key(peer);
        match connect_h3(peer).await {
            Ok((session, reused)) => {
                ORIGIN_H3_STATE_MANAGER.record_success(key);
                Ok((Connection::Session(session), reused))
            }
            Err(err) => {
                ORIGIN_H3_STATE_MANAGER.record_failure(key);
                Err(err)
            }
        }
    }

    async fn reused_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        peer: &P,
    ) -> Option<Self::Session> {
        let plan = match connection_plan(peer) {
            Ok(plan) => plan,
            Err(error) => {
                tracing::debug!(%error, "HTTP/3 peer is not eligible for idle connection reuse");
                return None;
            }
        };
        let context = crate::origin_h3_pool::take(&plan.key)?;
        Some(session_for_connection(peer, context))
    }

    async fn release_http_session<P: Peer + Send + Sync + 'static>(
        &self,
        mut session: Self::Session,
        _peer: &P,
        _idle_timeout: Option<Duration>,
    ) {
        if let Err(error) = session.finish_custom().await {
            tracing::warn!(%error, "HTTP/3 origin request could not finish; connection not pooled");
            return;
        }
        if session.can_reuse_connection() {
            let context = session.context.clone();
            crate::origin_h3_pool::offer(context.key.clone(), context);
        }
    }
}

pub struct OriginH3ClientSession {
    context: Arc<OriginH3Connection>,
    request_send: Option<Arc<H3RequestSend>>,
    response_stream: Option<Mutex<H3ResponseStream>>,
    response_header: Option<ResponseHeader>,
    response_finished: bool,
    response_trailers_finished: bool,
    failed: bool,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    digest: Digest,
    server_addr: SocketAddr,
    client_addr: SocketAddr,
}

impl OriginH3ClientSession {
    fn can_reuse_connection(&self) -> bool {
        self.context.is_open()
            && !self.failed
            && self.request_send.as_ref().is_some_and(|send| {
                send.finished.load(Ordering::Acquire) && !send.failed.load(Ordering::Acquire)
            })
            && self
                .response_header
                .as_ref()
                .is_some_and(|header| header.status.as_u16() >= 200)
            && self.response_finished
            && self.response_trailers_finished
            && self.response_stream.is_none()
    }
}

/// T4-6: build the H3 endpoint on the selected upstream path. Kernel
/// mode keeps the existing bound `std::net::UdpSocket`; `afxdp` mode
/// dials a node-originated AF_XDP UDP flow and hands quinn an abstract
/// socket over it — an explicit failure surfaces as `ConnectError`
/// (never a silent kernel fallback).
async fn create_h3_endpoint(plan: &H3ConnectionPlan) -> PingoraResult<Endpoint> {
    let server_addr = plan.server_addr;
    #[cfg(target_os = "linux")]
    // Loopback upstreams take the kernel path — `lo` has no XSK/reactor
    // queue, so an AF_XDP dial to one can only fail.
    if plan.key.af_registry_identity.is_some() {
        let registry = plan.registry.as_ref().ok_or_else(|| {
            Error::explain(
                ErrorType::ConnectError,
                "XDP enabled but no live AF_XDP registry for HTTP/3 origin",
            )
        })?;
        let socket = registry.dial_udp(server_addr, None).await.map_err(|err| {
            Error::explain(
                ErrorType::ConnectError,
                format!("AF_XDP H3 UDP dial to {server_addr}: {err}"),
            )
        })?;
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        return Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            None,
            Arc::new(af_xdp_quinn::AfXdpQuinnUdpSocket { socket }),
            runtime,
        )
        .map_err(|err| {
            Error::explain(
                ErrorType::ConnectError,
                format!("creating AF_XDP H3 endpoint to {server_addr}: {err}"),
            )
        });
    }

    let bind_addr = match server_addr.ip() {
        IpAddr::V4(_) => StdSocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
        IpAddr::V6(_) => StdSocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0),
    };
    Endpoint::client(bind_addr).map_err(|err| {
        Error::explain(
            ErrorType::ConnectError,
            format!("creating H3 endpoint: {err}"),
        )
    })
}

/// T4-6: quinn `AsyncUdpSocket` over a node-dialed AF_XDP UDP flow.
/// Sends and receives ride the flow's bounded channels — a full egress
/// queue reports `WouldBlock` and the poller awaits channel capacity,
/// so quinn's pacing keeps its semantics without silent drops.
#[cfg(target_os = "linux")]
mod af_xdp_quinn {
    use std::future::Future;
    use std::io::{self, IoSliceMut};
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    use quinn::udp::{RecvMeta, Transmit};
    use quinn::{AsyncUdpSocket, UdpPoller};

    use crate::xdp::af_xdp::{AfXdpReactorRequest, AfXdpUdpSocket};

    pub(super) struct AfXdpQuinnUdpSocket {
        pub(super) socket: AfXdpUdpSocket,
    }

    impl std::fmt::Debug for AfXdpQuinnUdpSocket {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AfXdpQuinnUdpSocket")
                .field("socket", &self.socket)
                .finish()
        }
    }

    /// Writable poller over the flow's bounded egress channel. Capacity
    /// is awaited via `reserve_owned`; the returned permit is dropped
    /// immediately — it only proves a slot existed, and quinn retries
    /// `try_send` which re-registers here if the slot was reclaimed.
    type PendingPermit = Pin<
        Box<
            dyn Future<
                    Output = Result<
                        tokio::sync::mpsc::OwnedPermit<AfXdpReactorRequest>,
                        tokio::sync::mpsc::error::SendError<()>,
                    >,
                > + Send,
        >,
    >;

    struct AfXdpUdpPoller {
        tx: tokio::sync::mpsc::Sender<AfXdpReactorRequest>,
        // `Mutex` keeps the poller `Sync` (`UdpPoller` requires it);
        // `poll_writable` only ever holds the guard while polling.
        pending: std::sync::Mutex<Option<PendingPermit>>,
    }

    impl std::fmt::Debug for AfXdpUdpPoller {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AfXdpUdpPoller").finish_non_exhaustive()
        }
    }

    impl UdpPoller for AfXdpUdpPoller {
        fn poll_writable(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            let mut pending = this.pending.lock().unwrap_or_else(|err| err.into_inner());
            if pending.is_none() {
                let tx = this.tx.clone();
                *pending = Some(Box::pin(async move { tx.reserve_owned().await }));
            }
            match pending.as_mut().expect("pending future").as_mut().poll(cx) {
                Poll::Ready(Ok(_permit)) => {
                    *pending = None;
                    Poll::Ready(Ok(()))
                }
                Poll::Ready(Err(_)) => {
                    // Clear the resolved future — a completed
                    // `reserve_owned` must never be polled again.
                    *pending = None;
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "AF_XDP UDP egress queue closed",
                    )))
                }
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl AsyncUdpSocket for AfXdpQuinnUdpSocket {
        fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
            Box::pin(AfXdpUdpPoller {
                tx: self.socket.egress_sender(),
                pending: std::sync::Mutex::new(None),
            })
        }

        fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
            if transmit.destination != self.socket.peer_addr() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "AF_XDP QUIC transmit destination {} mismatches dialed peer {}",
                        transmit.destination,
                        self.socket.peer_addr()
                    ),
                ));
            }
            if transmit.segment_size.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AF_XDP QUIC socket does not support GSO segmentation",
                ));
            }
            self.socket
                .try_send(transmit.contents, transmit.ecn.map(|ecn| ecn as u8))
                .map(|_| ())
        }

        fn poll_recv(
            &self,
            cx: &mut Context<'_>,
            bufs: &mut [IoSliceMut<'_>],
            meta: &mut [RecvMeta],
        ) -> Poll<io::Result<usize>> {
            if bufs.is_empty() || meta.is_empty() {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AF_XDP QUIC recv requires at least one buffer",
                )));
            }
            match self.socket.poll_recv(cx, &mut bufs[0]) {
                Poll::Ready(Ok((len, ecn))) => {
                    meta[0] = RecvMeta {
                        addr: self.socket.peer_addr(),
                        len,
                        stride: len,
                        // IP-header ECN bits parsed by the AF_XDP
                        // demux — QUIC keeps its congestion feedback.
                        ecn: ecn.and_then(quinn::udp::EcnCodepoint::from_bits),
                        dst_ip: Some(self.socket.local_addr().ip()),
                    };
                    Poll::Ready(Ok(1))
                }
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Pending => Poll::Pending,
            }
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(self.socket.local_addr())
        }

        // The userspace dataplane never IP-fragments egress frames, so
        // quinn keeps path-MTU discovery armed.
        fn may_fragment(&self) -> bool {
            false
        }
    }
}

async fn connect_h3<P: Peer + Send + Sync + 'static>(
    peer: &P,
) -> PingoraResult<(OriginH3ClientSession, bool)> {
    let plan = connection_plan(peer)?;
    if let Some(context) = crate::origin_h3_pool::take(&plan.key) {
        return Ok((session_for_connection(peer, context), true));
    }

    let server_addr = plan.server_addr;
    let mut endpoint = create_h3_endpoint(&plan).await?;
    endpoint.set_default_client_config(client_config(peer.verify_cert(), plan.transport.as_ref())?);

    let sni = peer.sni();
    let connecting = endpoint.connect(server_addr, sni).map_err(|err| {
        Error::explain(
            ErrorType::ConnectError,
            format!("starting H3 connect: {err}"),
        )
    })?;
    let connection = match peer.connection_timeout() {
        Some(timeout) => tokio::time::timeout(timeout, connecting)
            .await
            .map_err(|_| {
                Error::explain(ErrorType::ConnectTimedout, "connecting to HTTP/3 origin")
            })?,
        None => connecting.await,
    }
    .map_err(|err| {
        Error::explain(
            ErrorType::ConnectError,
            format!("connecting to HTTP/3 origin: {err}"),
        )
    })?;

    let context = build_h3_connection(plan.key, endpoint, connection).await?;
    Ok((session_for_connection(peer, context), false))
}

async fn build_h3_connection(
    key: OriginH3PoolKey,
    endpoint: Endpoint,
    connection: quinn::Connection,
) -> PingoraResult<Arc<OriginH3Connection>> {
    let server_addr = connection.remote_address();
    let local_addr = endpoint.local_addr().map_err(|error| {
        Error::explain(
            ErrorType::ConnectError,
            format!("reading H3 endpoint local address: {error}"),
        )
    })?;
    let h3_conn = h3_quinn::Connection::new(connection.clone());
    let (mut driver, send_request) = h3::client::builder().build(h3_conn).await.map_err(|err| {
        Error::explain(
            ErrorType::ConnectError,
            format!("starting H3 client: {err}"),
        )
    })?;
    let driver = tokio::spawn(async move {
        let reason = futures_util::future::poll_fn(|cx| driver.poll_close(cx)).await;
        tracing::debug!(%reason, "HTTP/3 origin connection driver closed");
    });

    Ok(Arc::new(OriginH3Connection {
        key,
        _endpoint: endpoint,
        quic: connection,
        sender: Mutex::new(send_request),
        driver,
        server_addr,
        client_addr: local_addr,
    }))
}

fn session_for_connection<P: Peer>(
    peer: &P,
    context: Arc<OriginH3Connection>,
) -> OriginH3ClientSession {
    let server_addr = context.server_addr.into();
    let client_addr = context.client_addr.into();
    OriginH3ClientSession {
        context,
        request_send: None,
        response_stream: None,
        response_header: None,
        response_finished: false,
        response_trailers_finished: false,
        failed: false,
        read_timeout: peer.get_peer_options().and_then(|opts| opts.read_timeout),
        write_timeout: peer.get_peer_options().and_then(|opts| opts.write_timeout),
        digest: Digest::default(),
        server_addr,
        client_addr,
    }
}

fn client_config(
    verify_cert: bool,
    _transport: Option<&crate::runtime_mode::XdpTransportSettings>,
) -> PingoraResult<ClientConfig> {
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        crate::tls_crypto::default_crypto_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .map_err(|error| {
        Error::explain(
            ErrorType::ConnectError,
            format!("building H3 TLS provider: {error}"),
        )
    })?;
    let mut crypto = if verify_cert {
        let mut roots = rustls::RootCertStore::empty();
        let certs = rustls_native_certs::load_native_certs();
        for error in certs.errors {
            tracing::warn!(%error, "loading native root certificates for H3 origin");
        }
        for cert in certs.certs {
            if let Err(error) = roots.add(cert) {
                tracing::warn!(%error, "native root certificate rejected for H3 origin");
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    } else {
        builder
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth()
    };
    crypto.alpn_protocols = vec![b"h3".to_vec()];
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(|err| {
        Error::explain(
            ErrorType::ConnectError,
            format!("building H3 TLS config: {err}"),
        )
    })?;
    let config = ClientConfig::new(Arc::new(quic));
    // T5: when this client will run over the AF_XDP upstream path,
    // apply the policy-selected transport controller. The stock
    // controller stays for kernel-socket H3 (non-XDP contract
    // unchanged) and for the cubic default.
    #[cfg(target_os = "linux")]
    let config = {
        let mut config = config;
        if let Some(settings) = _transport {
            let mut transport = crate::quic_transport::tuned_transport_config(None);
            transport.congestion_controller_factory(Arc::new(
                crate::quic_cc::XdpTransportControllerFactory::new(settings.clone()),
            ));
            config.transport_config(Arc::new(transport));
        }
        config
    };
    Ok(config)
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(crate::tls_crypto::default_crypto_provider())))
    }
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[async_trait]
impl CustomClientSession for OriginH3ClientSession {
    async fn write_request_header(
        &mut self,
        req: Box<RequestHeader>,
        end: bool,
    ) -> PingoraResult<()> {
        let request = build_h3_request(&req)?;
        let mut send_request = self.context.sender.lock().await;
        let result = async {
            match self.write_timeout {
                Some(timeout) => tokio::time::timeout(timeout, send_request.send_request(request))
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::WriteTimedout, "sending HTTP/3 request header")
                    })?,
                None => send_request.send_request(request).await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::WriteError,
                    format!("sending HTTP/3 request header: {err}"),
                )
            })
        }
        .await;
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        drop(send_request);

        let (send, recv) = stream.split();
        let send = Arc::new(H3RequestSend {
            stream: Mutex::new(send),
            finished: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        });
        self.request_send = Some(send.clone());
        self.response_stream = Some(Mutex::new(recv));
        if end {
            finish_h3_body(send, self.write_timeout).await?;
        }
        Ok(())
    }

    async fn write_request_body(&mut self, data: Bytes, end: bool) -> PingoraResult<()> {
        let Some(stream) = self.request_send.as_ref().cloned() else {
            return Error::e_explain(
                ErrorType::WriteError,
                "HTTP/3 request stream is not initialized",
            );
        };
        write_h3_body(stream, data, end, self.write_timeout).await
    }

    async fn finish_request_body(&mut self) -> PingoraResult<()> {
        let Some(stream) = self.request_send.as_ref().cloned() else {
            return Ok(());
        };
        finish_h3_body(stream, self.write_timeout).await?;
        Ok(())
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    async fn read_response_header(&mut self) -> PingoraResult<()> {
        let Some(stream) = self.response_stream.as_mut() else {
            return Error::e_explain(
                ErrorType::ReadError,
                "HTTP/3 request stream is not initialized",
            );
        };
        let stream = stream.get_mut();
        let result = async {
            match self.read_timeout {
                Some(timeout) => tokio::time::timeout(timeout, stream.recv_response())
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::ReadTimedout, "reading HTTP/3 response header")
                    })?,
                None => stream.recv_response().await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::ReadError,
                    format!("reading HTTP/3 response header: {err}"),
                )
            })
        }
        .await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        self.response_header = Some(build_response_header(response)?);
        Ok(())
    }

    async fn read_response_body(&mut self) -> PingoraResult<Option<Bytes>> {
        if self.response_finished {
            return Ok(None);
        }
        let Some(stream) = self.response_stream.as_mut() else {
            return Error::e_explain(
                ErrorType::ReadError,
                "HTTP/3 request stream is not initialized",
            );
        };
        let stream = stream.get_mut();
        let result = async {
            match self.read_timeout {
                Some(timeout) => tokio::time::timeout(timeout, stream.recv_data())
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::ReadTimedout, "reading HTTP/3 response body")
                    })?,
                None => stream.recv_data().await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::ReadError,
                    format!("reading HTTP/3 response body: {err}"),
                )
            })
        }
        .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        let Some(mut chunk) = result else {
            self.response_finished = true;
            return Ok(None);
        };
        let len = chunk.remaining();
        Ok(Some(chunk.copy_to_bytes(len)))
    }

    fn response_finished(&self) -> bool {
        self.response_finished
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        self.failed = true;
        if let Some(send) = self.request_send.as_ref() {
            send.failed.store(true, Ordering::Release);
            send.stream
                .lock()
                .await
                .stop_stream(Code::H3_REQUEST_CANCELLED);
        }
        if let Some(recv) = self.response_stream.as_mut() {
            recv.get_mut().stop_sending(Code::H3_REQUEST_CANCELLED);
        }
    }

    fn response_header(&self) -> Option<&ResponseHeader> {
        self.response_header.as_ref()
    }

    fn was_upgraded(&self) -> bool {
        false
    }

    fn digest(&self) -> Option<&Digest> {
        Some(&self.digest)
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        Some(&mut self.digest)
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        Some(&self.server_addr)
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        Some(&self.client_addr)
    }

    async fn read_trailers(&mut self) -> PingoraResult<Option<HeaderMap>> {
        let Some(stream) = self.response_stream.as_mut() else {
            return Ok(None);
        };
        let stream = stream.get_mut();
        let result = async {
            match self.read_timeout {
                Some(timeout) => tokio::time::timeout(timeout, stream.recv_trailers())
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::ReadTimedout, "reading HTTP/3 response trailers")
                    })?,
                None => stream.recv_trailers().await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::ReadError,
                    format!("reading HTTP/3 trailers: {err}"),
                )
            })
        }
        .await;
        match result {
            Ok(trailers) => {
                self.response_trailers_finished = true;
                self.response_stream = None;
                Ok(trailers)
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    fn fd(&self) -> UniqueIDType {
        0
    }

    async fn check_response_end_or_error(&mut self, headers: bool) -> PingoraResult<bool> {
        let no_body = headers
            && self
                .response_header
                .as_ref()
                .is_some_and(|header| matches!(header.status.as_u16(), 204 | 304));
        if no_body {
            self.response_finished = true;
        }
        Ok(no_body)
    }

    fn take_request_body_writer(&mut self) -> Option<Box<dyn BodyWrite>> {
        self.request_send.as_ref().map(|stream| {
            Box::new(OriginH3BodyWriter {
                stream: stream.clone(),
                write_timeout: self.write_timeout,
            }) as Box<dyn BodyWrite>
        })
    }

    async fn finish_custom(&mut self) -> PingoraResult<()> {
        self.finish_request_body().await
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = PingoraResult<Bytes>> + Unpin + Send + Sync + 'static>> {
        Some(Box::new(futures_util::stream::empty()))
    }

    async fn drain_custom_messages(&mut self) -> PingoraResult<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }
}

struct OriginH3BodyWriter {
    stream: Arc<H3RequestSend>,
    write_timeout: Option<Duration>,
}

impl Drop for OriginH3BodyWriter {
    fn drop(&mut self) {
        if !self.stream.finished.load(Ordering::Acquire) {
            self.stream.failed.store(true, Ordering::Release);
        }
    }
}

#[async_trait]
impl BodyWrite for OriginH3BodyWriter {
    async fn write_all_buf(&mut self, data: &mut Bytes) -> PingoraResult<()> {
        if data.is_empty() {
            return Ok(());
        }
        let bytes = data.split_to(data.len());
        write_h3_body(self.stream.clone(), bytes, false, self.write_timeout).await
    }

    async fn finish(&mut self) -> PingoraResult<()> {
        finish_h3_body(self.stream.clone(), self.write_timeout).await
    }

    async fn cleanup(&mut self) -> PingoraResult<()> {
        if !self.stream.finished.load(Ordering::Acquire) {
            let mut stream = self.stream.stream.lock().await;
            if !self.stream.finished.load(Ordering::Acquire) {
                self.stream.failed.store(true, Ordering::Release);
                stream.stop_stream(Code::H3_REQUEST_CANCELLED);
            }
        }
        Ok(())
    }

    fn upgrade_body_writer(&mut self) {}
}

async fn write_h3_body(
    send: Arc<H3RequestSend>,
    data: Bytes,
    end: bool,
    write_timeout: Option<Duration>,
) -> PingoraResult<()> {
    let mut stream = send.stream.lock().await;
    if send.failed.load(Ordering::Acquire) {
        return Error::e_explain(ErrorType::WriteError, "HTTP/3 request body writer failed");
    }
    if send.finished.load(Ordering::Acquire) {
        return if data.is_empty() {
            Ok(())
        } else {
            Error::e_explain(
                ErrorType::WriteError,
                "HTTP/3 request body already finished",
            )
        };
    }
    let result = async {
        if !data.is_empty() {
            match write_timeout {
                Some(timeout) => tokio::time::timeout(timeout, stream.send_data(data))
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::WriteTimedout, "writing HTTP/3 request body")
                    })?,
                None => stream.send_data(data).await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::WriteError,
                    format!("writing HTTP/3 request body: {err}"),
                )
            })?;
        }
        if end {
            match write_timeout {
                Some(timeout) => tokio::time::timeout(timeout, stream.finish())
                    .await
                    .map_err(|_| {
                        Error::explain(ErrorType::WriteTimedout, "finishing HTTP/3 request body")
                    })?,
                None => stream.finish().await,
            }
            .map_err(|err| {
                Error::explain(
                    ErrorType::WriteError,
                    format!("finishing HTTP/3 request body: {err}"),
                )
            })?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        send.failed.store(true, Ordering::Release);
    } else if end {
        send.finished.store(true, Ordering::Release);
    }
    result
}

async fn finish_h3_body(
    stream: Arc<H3RequestSend>,
    write_timeout: Option<Duration>,
) -> PingoraResult<()> {
    write_h3_body(stream, Bytes::new(), true, write_timeout).await
}

fn build_h3_request(req: &RequestHeader) -> PingoraResult<http::Request<()>> {
    let path = std::str::from_utf8(req.raw_path()).unwrap_or("/");
    let authority = req
        .headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_else(|| req.uri.host().unwrap_or("localhost"));
    let uri = format!("https://{authority}{path}")
        .parse::<http::Uri>()
        .map_err(|err| {
            Error::explain(
                ErrorType::InvalidHTTPHeader,
                format!("building HTTP/3 request URI: {err}"),
            )
        })?;
    let mut builder = http::Request::builder()
        .method(req.method.clone())
        .uri(uri)
        .version(Version::HTTP_3);
    for (name, value) in &req.headers {
        if is_h3_forbidden_header(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder.body(()).map_err(|err| {
        Error::explain(
            ErrorType::InvalidHTTPHeader,
            format!("building HTTP/3 request: {err}"),
        )
    })
}

fn build_response_header(response: Response<()>) -> PingoraResult<ResponseHeader> {
    let (parts, _) = response.into_parts();
    let mut header = ResponseHeader::build(parts.status, Some(parts.headers.len()))?;
    header.set_version(Version::HTTP_3);
    // The owning HeaderMap iterator uses None for subsequent values of the
    // same name. Borrowed iteration preserves every field, including Set-Cookie.
    for (name, value) in &parts.headers {
        if !is_h3_forbidden_header(name) {
            header.append_header(name.clone(), value.clone())?;
        }
    }
    Ok(header)
}

fn is_h3_forbidden_header(name: &http::header::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn origin_h3_key<P: Peer>(peer: &P) -> OriginH3Key {
    OriginH3Key::Target {
        addr: peer.address().to_string(),
        sni: peer.sni().to_string(),
    }
}

pub fn should_try_origin_h3_for_peer<P: Peer>(peer: &P) -> bool {
    ORIGIN_H3_STATE_MANAGER.should_try_h3(&origin_h3_key(peer))
}

#[cfg(test)]
#[path = "origin_h3_tests.rs"]
mod wire_tests;
