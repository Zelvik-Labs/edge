use std::net::SocketAddr as StdSocketAddr;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::Stream;
use h3::error::Code;
use http::{HeaderMap, Request, Response, Version};
use pingora_core::protocols::Digest;
use pingora_core::protocols::http::HttpTask;
use pingora_core::protocols::http::body_buffer::FixedBuffer;
use pingora_core::protocols::http::custom::CustomMessageWrite;
use pingora_core::protocols::http::custom::server::Session as CustomServerSession;
use pingora_core::protocols::l4::socket::SocketAddr;
use pingora_core::{Error, ErrorType, Result as PingoraResult};
use pingora_http::{RequestHeader, ResponseHeader};
use tokio::sync::Mutex;

pub struct H3DownstreamSession<S>
where
    S: h3::quic::BidiStream<Bytes> + Send + 'static,
{
    req_header: RequestHeader,
    stream: Mutex<h3::server::RequestStream<S, Bytes>>,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    digest: Digest,
    response_written: Option<ResponseHeader>,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    total_drain_timeout: Option<Duration>,
    body_done: bool,
    data_done: bool,
    trailers_seen: bool,
    prefetched_body: Option<Bytes>,
    body_error: Option<Box<Error>>,
    response_done: bool,
    body_bytes_read: usize,
    body_bytes_sent: usize,
    retry_buffer: Option<FixedBuffer>,
}

impl<S> H3DownstreamSession<S>
where
    S: h3::quic::BidiStream<Bytes> + Send + 'static,
{
    pub fn new(
        request: Request<()>,
        stream: h3::server::RequestStream<S, Bytes>,
        remote_addr: StdSocketAddr,
        local_addr: StdSocketAddr,
    ) -> PingoraResult<Self> {
        let req_header = build_request_header(request)?;
        Ok(Self {
            req_header,
            stream: Mutex::new(stream),
            client_addr: remote_addr.into(),
            server_addr: local_addr.into(),
            digest: Digest::default(),
            response_written: None,
            read_timeout: None,
            write_timeout: None,
            total_drain_timeout: None,
            body_done: false,
            data_done: false,
            trailers_seen: false,
            prefetched_body: None,
            body_error: None,
            response_done: false,
            body_bytes_read: 0,
            body_bytes_sent: 0,
            retry_buffer: None,
        })
    }

    // poll_recv_data(None) can mean trailers rather than FIN. Complete the
    // trailer parser too, so delayed or malformed trailing frames never prove
    // that an HTTP/3 request is safely bodyless for a shared cache key.
    fn poll_body_chunk(&mut self, cx: &mut Context<'_>) -> Poll<PingoraResult<Option<Bytes>>> {
        let stream = self.stream.get_mut();
        if !self.data_done {
            match std::task::ready!(stream.poll_recv_data(cx)) {
                Ok(Some(mut chunk)) => {
                    let len = chunk.remaining();
                    return Poll::Ready(Ok(Some(chunk.copy_to_bytes(len))));
                }
                Ok(None) => self.data_done = true,
                Err(err) => return Poll::Ready(Err(Self::body_read_error(err))),
            }
        }
        match std::task::ready!(stream.poll_recv_trailers(cx)) {
            Ok(trailers) => {
                self.trailers_seen = trailers.is_some();
                self.body_done = true;
                Poll::Ready(Ok(None))
            }
            Err(err) => Poll::Ready(Err(Self::body_read_error(err))),
        }
    }

    fn body_read_error(err: h3::error::StreamError) -> Box<Error> {
        Error::explain(
            ErrorType::ReadError,
            format!("reading HTTP/3 request body: {err}"),
        )
    }

    async fn recv_body_chunk(&mut self) -> PingoraResult<Option<Bytes>> {
        if let Some(err) = self.body_error.take() {
            return Err(err);
        }
        let bytes = if let Some(bytes) = self.prefetched_body.take() {
            bytes
        } else {
            if self.body_done {
                return Ok(None);
            }
            let timeout = self.read_timeout;
            let receive = futures_util::future::poll_fn(|cx| self.poll_body_chunk(cx));
            let chunk = if let Some(timeout) = timeout {
                tokio::time::timeout(timeout, receive).await.map_err(|_| {
                    Error::explain(ErrorType::ReadTimedout, "reading HTTP/3 request body")
                })??
            } else {
                receive.await?
            };
            let Some(bytes) = chunk else {
                return Ok(None);
            };
            bytes
        };
        self.body_bytes_read = self.body_bytes_read.saturating_add(bytes.len());
        if let Some(buffer) = self.retry_buffer.as_mut() {
            buffer.write_to_buffer(&bytes);
        }
        Ok(Some(bytes))
    }

    async fn send_response(&mut self, resp: &ResponseHeader, end: bool) -> PingoraResult<()> {
        let response = build_h3_response(resp)?;
        let stream = self.stream.get_mut();
        let send_result = if let Some(timeout) = self.write_timeout {
            tokio::time::timeout(timeout, stream.send_response(response))
                .await
                .map_err(|_| {
                    Error::explain(ErrorType::WriteTimedout, "writing HTTP/3 response header")
                })?
        } else {
            stream.send_response(response).await
        };
        send_result.map_err(|err| {
            Error::explain(
                ErrorType::WriteError,
                format!("writing HTTP/3 response header: {err}"),
            )
        })?;

        self.response_written = Some(resp.clone());
        if end {
            self.finish().await?;
        }
        Ok(())
    }

    async fn send_body(&mut self, data: Bytes) -> PingoraResult<()> {
        self.body_bytes_sent = self.body_bytes_sent.saturating_add(data.len());
        let stream = self.stream.get_mut();
        let send_result = if let Some(timeout) = self.write_timeout {
            tokio::time::timeout(timeout, stream.send_data(data))
                .await
                .map_err(|_| {
                    Error::explain(ErrorType::WriteTimedout, "writing HTTP/3 response body")
                })?
        } else {
            stream.send_data(data).await
        };
        send_result.map_err(|err| {
            Error::explain(
                ErrorType::WriteError,
                format!("writing HTTP/3 response body: {err}"),
            )
        })
    }
}

#[async_trait]
impl<S> CustomServerSession for H3DownstreamSession<S>
where
    S: h3::quic::BidiStream<Bytes> + Send + 'static,
{
    fn req_header(&self) -> &RequestHeader {
        &self.req_header
    }

    fn req_header_mut(&mut self) -> &mut RequestHeader {
        &mut self.req_header
    }

    async fn read_body_bytes(&mut self) -> PingoraResult<Option<Bytes>> {
        self.recv_body_chunk().await
    }

    async fn drain_request_body(&mut self) -> PingoraResult<()> {
        let timeout = self.total_drain_timeout;
        let drain = async {
            while self.recv_body_chunk().await?.is_some() {}
            Ok(())
        };
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, drain).await.map_err(|_| {
                Error::explain(ErrorType::ReadTimedout, "draining HTTP/3 request body")
            })?
        } else {
            drain.await
        }
    }

    async fn write_response_header(
        &mut self,
        resp: Box<ResponseHeader>,
        end: bool,
    ) -> PingoraResult<()> {
        self.send_response(&resp, end).await
    }

    async fn write_response_header_ref(
        &mut self,
        resp: &ResponseHeader,
        end: bool,
    ) -> PingoraResult<()> {
        self.send_response(resp, end).await
    }

    async fn write_body(&mut self, data: Bytes, end: bool) -> PingoraResult<()> {
        if !data.is_empty() {
            self.send_body(data).await?;
        }
        if end {
            self.finish().await?;
        }
        Ok(())
    }

    async fn write_trailers(&mut self, trailers: HeaderMap) -> PingoraResult<()> {
        self.stream
            .get_mut()
            .send_trailers(trailers)
            .await
            .map_err(|err| {
                Error::explain(
                    ErrorType::WriteError,
                    format!("writing HTTP/3 trailers: {err}"),
                )
            })?;
        self.finish().await
    }

    async fn response_duplex_vec(&mut self, tasks: Vec<HttpTask>) -> PingoraResult<bool> {
        let mut done = false;
        for task in tasks {
            match task {
                HttpTask::Header(resp, end) => {
                    self.write_response_header(resp, end).await?;
                    done |= end;
                }
                HttpTask::Body(Some(data), end) | HttpTask::UpgradedBody(Some(data), end) => {
                    self.write_body(data, end).await?;
                    done |= end;
                }
                HttpTask::Body(None, end) | HttpTask::UpgradedBody(None, end) => {
                    if end {
                        self.finish().await?;
                        done = true;
                    }
                }
                HttpTask::Trailer(Some(trailers)) => {
                    self.write_trailers(*trailers).await?;
                    done = true;
                }
                HttpTask::Trailer(None) | HttpTask::Done => {
                    self.finish().await?;
                    done = true;
                }
                HttpTask::Failed(err) => return Err(err),
            }
        }
        Ok(done)
    }

    fn set_read_timeout(&mut self, timeout: Option<Duration>) {
        self.read_timeout = timeout;
    }

    fn get_read_timeout(&self) -> Option<Duration> {
        self.read_timeout
    }

    fn set_write_timeout(&mut self, timeout: Option<Duration>) {
        self.write_timeout = timeout;
    }

    fn get_write_timeout(&self) -> Option<Duration> {
        self.write_timeout
    }

    fn set_total_drain_timeout(&mut self, timeout: Option<Duration>) {
        self.total_drain_timeout = timeout;
    }

    fn get_total_drain_timeout(&self) -> Option<Duration> {
        self.total_drain_timeout
    }

    fn request_summary(&self) -> String {
        let host = self
            .req_header
            .headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-");
        format!("{} {} {host}", self.req_header.method, self.req_header.uri)
    }

    fn response_written(&self) -> Option<&ResponseHeader> {
        self.response_written.as_ref()
    }

    async fn shutdown(&mut self, _code: u32, _ctx: &str) {
        self.stream
            .get_mut()
            .stop_stream(Code::H3_REQUEST_CANCELLED);
    }

    fn is_body_done(&mut self) -> bool {
        self.body_done
    }

    async fn finish(&mut self) -> PingoraResult<()> {
        if self.response_done {
            return Ok(());
        }
        self.stream.get_mut().finish().await.map_err(|err| {
            Error::explain(
                ErrorType::WriteError,
                format!("finishing HTTP/3 response: {err}"),
            )
        })?;
        self.response_done = true;
        Ok(())
    }

    fn is_body_empty(&mut self) -> bool {
        if self.body_bytes_read != 0
            || self.prefetched_body.is_some()
            || self.body_error.is_some()
            || self.trailers_seen
        {
            return false;
        }
        if self.body_done {
            return true;
        }
        // Cache admission is synchronous. Probe only already available frames;
        // never wait for an upload or infer FIN from GET/Content-Length: 0.
        // A later read polls again with its own waker. Any prefetched bytes or
        // error remain owned by this session and are delivered exactly once.
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll_body_chunk(&mut cx) {
            Poll::Ready(Ok(None)) => !self.trailers_seen,
            Poll::Ready(Ok(Some(bytes))) => {
                self.prefetched_body = Some(bytes);
                false
            }
            Poll::Ready(Err(err)) => {
                self.body_error = Some(err);
                false
            }
            Poll::Pending => false,
        }
    }

    async fn read_body_or_idle(&mut self, no_body_expected: bool) -> PingoraResult<Option<Bytes>> {
        if no_body_expected || self.is_body_done() {
            std::future::pending().await
        } else {
            self.read_body_bytes().await
        }
    }

    fn body_bytes_sent(&self) -> usize {
        self.body_bytes_sent
    }

    fn body_bytes_read(&self) -> usize {
        self.body_bytes_read
    }

    fn digest(&self) -> Option<&Digest> {
        Some(&self.digest)
    }

    fn digest_mut(&mut self) -> Option<&mut Digest> {
        Some(&mut self.digest)
    }

    fn client_addr(&self) -> Option<&SocketAddr> {
        Some(&self.client_addr)
    }

    fn server_addr(&self) -> Option<&SocketAddr> {
        Some(&self.server_addr)
    }

    fn pseudo_raw_h1_request_header(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.extend_from_slice(self.req_header.method.as_str().as_bytes());
        out.extend_from_slice(b" ");
        out.extend_from_slice(self.req_header.raw_path());
        out.extend_from_slice(b" HTTP/3\r\n");
        for (name, value) in &self.req_header.headers {
            out.extend_from_slice(name.as_str().as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.freeze()
    }

    fn enable_retry_buffering(&mut self) {
        self.retry_buffer = Some(FixedBuffer::new(64 * 1024));
    }

    fn retry_buffer_truncated(&self) -> bool {
        self.retry_buffer
            .as_ref()
            .is_some_and(FixedBuffer::is_truncated)
    }

    fn get_retry_buffer(&self) -> Option<Bytes> {
        let buffer = self.retry_buffer.as_ref()?;
        if buffer.is_truncated() {
            return None;
        }
        // Preserve Pingora's initial body-pipe end marker after a FIN/trailer
        // probe. The request-body filter normalizes empty final data before
        // writing upstream; a truncated body must never become a replay.
        Some(buffer.get_buffer().unwrap_or_default())
    }

    async fn finish_custom(&mut self) -> PingoraResult<()> {
        self.finish().await
    }

    fn take_custom_message_reader(
        &mut self,
    ) -> Option<Box<dyn Stream<Item = PingoraResult<Bytes>> + Unpin + Send + Sync + 'static>> {
        Some(Box::new(futures_util::stream::empty()))
    }

    fn restore_custom_message_reader(
        &mut self,
        _reader: Box<dyn Stream<Item = PingoraResult<Bytes>> + Unpin + Send + Sync + 'static>,
    ) -> PingoraResult<()> {
        Ok(())
    }

    fn take_custom_message_writer(&mut self) -> Option<Box<dyn CustomMessageWrite>> {
        Some(Box::new(()))
    }

    fn restore_custom_message_writer(
        &mut self,
        _writer: Box<dyn CustomMessageWrite>,
    ) -> PingoraResult<()> {
        Ok(())
    }
}

fn build_request_header(request: Request<()>) -> PingoraResult<RequestHeader> {
    let (parts, _) = request.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let mut req =
        RequestHeader::build_no_case(parts.method, path.as_bytes(), Some(parts.headers.len()))?;
    req.set_version(Version::HTTP_11);

    if let Some(authority) = parts.uri.authority()
        && !parts.headers.contains_key(http::header::HOST)
    {
        req.insert_header(http::header::HOST, authority.as_str())?;
    }

    for (name, value) in &parts.headers {
        if !is_h3_forbidden_response_header(name.as_str()) {
            req.append_header(name.clone(), value.clone())?;
        }
    }

    Ok(req)
}

fn build_h3_response(resp: &ResponseHeader) -> PingoraResult<Response<()>> {
    let mut builder = Response::builder().status(resp.status);
    for (name, value) in &resp.headers {
        if is_h3_forbidden_response_header(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder.body(()).map_err(|err| {
        Error::explain(
            ErrorType::InvalidHTTPHeader,
            format!("building HTTP/3 response: {err}"),
        )
    })
}

fn is_h3_forbidden_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Downstream = H3DownstreamSession<h3_quinn::BidiStream<Bytes>>;
    type ClientStream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

    struct Pair {
        _server: quinn::Endpoint,
        _client: quinn::Endpoint,
        _sender: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
        drivers: Vec<tokio::task::JoinHandle<()>>,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            for driver in &self.drivers {
                driver.abort();
            }
            self._client.close(0u32.into(), b"test complete");
            self._server.close(0u32.into(), b"test complete");
        }
    }

    async fn pair(request: Request<()>) -> (Downstream, ClientStream, Pair) {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let certs =
            rustls_pemfile::certs(&mut include_bytes!("../tests/testdata/h3/cert.pem").as_slice())
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
        let key = rustls_pemfile::private_key(
            &mut include_bytes!("../tests/testdata/h3/key.pem").as_slice(),
        )
        .unwrap()
        .unwrap();
        let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        server_tls.alpn_protocols = vec![b"h3".to_vec()];
        let server = quinn::Endpoint::server(
            quinn::ServerConfig::with_crypto(Arc::new(
                quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap(),
            )),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let addr = server.local_addr().unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(
            &mut include_bytes!("../pingora-main/pingora-core/examples/keys/server/cert.pem")
                .as_slice(),
        ) {
            roots.add(cert.unwrap()).unwrap();
        }
        let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap(),
        )));
        let (server_conn, client_conn) = tokio::join!(
            async { server.accept().await.unwrap().await.unwrap() },
            async {
                client
                    .connect(addr, "openrusty.org")
                    .unwrap()
                    .await
                    .unwrap()
            }
        );
        let remote = server_conn.remote_address();
        let server_builder = h3::server::builder();
        let (server_h3, client_h3) = tokio::join!(
            server_builder.build(h3_quinn::Connection::new(server_conn)),
            h3::client::new(h3_quinn::Connection::new(client_conn))
        );
        let mut server_h3 = server_h3.unwrap();
        let (mut client_driver, mut sender) = client_h3.unwrap();
        let client_driver = tokio::spawn(async move {
            let _ = client_driver.wait_idle().await;
        });
        let stream = sender.send_request(request).await.unwrap();
        let (request, downstream) = server_h3
            .accept()
            .await
            .unwrap()
            .unwrap()
            .resolve_request()
            .await
            .unwrap();
        let server_driver = tokio::spawn(async move {
            while let Ok(Some(resolver)) = server_h3.accept().await {
                let (_, mut stream) = resolver.resolve_request().await.unwrap();
                stream.stop_stream(Code::H3_REQUEST_REJECTED);
            }
        });
        (
            Downstream::new(request, downstream, remote, addr).unwrap(),
            stream,
            Pair {
                _server: server,
                _client: client,
                _sender: sender,
                drivers: vec![client_driver, server_driver],
            },
        )
    }

    fn get(content_length: Option<&str>) -> Request<()> {
        let mut request = Request::builder().uri("https://openrusty.org/cache");
        if let Some(length) = content_length {
            request = request.header("content-length", length);
        }
        request.body(()).unwrap()
    }

    async fn wait_for_probe(session: &mut Downstream) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                session.is_body_empty();
                if session.body_done
                    || session.prefetched_body.is_some()
                    || session.body_error.is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn h3_bodyless_cache_proof_requires_fin_not_method_or_content_length() {
        for length in [None, Some("0")] {
            let (mut session, mut client, _pair) = pair(get(length)).await;
            assert!(!session.is_body_empty());
            assert!(!session.is_body_done());
            client.finish().await.unwrap();
            wait_for_probe(&mut session).await;
            assert!(session.is_body_empty());
            assert!(session.is_body_done());
            assert!(session.read_body_bytes().await.unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn h3_cache_probe_preserves_body_and_retry_accounting_even_with_false_zero_length() {
        for length in [None, Some("0")] {
            let (mut session, mut client, _pair) = pair(get(length)).await;
            let body = Bytes::from(vec![0x5a; 16 * 1024]);
            client.send_data(body.clone()).await.unwrap();
            client.finish().await.unwrap();
            wait_for_probe(&mut session).await;
            assert!(!session.is_body_empty());
            assert_eq!(session.body_bytes_read(), 0);
            session.enable_retry_buffering();
            let mut received = Vec::new();
            while let Some(chunk) = session.read_body_bytes().await.unwrap() {
                received.extend_from_slice(&chunk);
            }
            assert_eq!(received.as_slice(), body.as_ref());
            assert_eq!(session.body_bytes_read(), body.len());
            assert_eq!(session.get_retry_buffer().unwrap(), body);
            assert!(!session.is_body_empty());
        }
    }

    #[tokio::test]
    async fn h3_cache_probe_does_not_admit_trailers_or_lose_resets() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        let mut trailers = HeaderMap::new();
        trailers.insert("authorization", "trailer-only".parse().unwrap());
        client.send_trailers(trailers).await.unwrap();
        client.finish().await.unwrap();
        wait_for_probe(&mut session).await;
        assert!(session.is_body_done());
        assert!(!session.is_body_empty());

        let (mut session, mut client, _pair) = pair(get(None)).await;
        client.stop_stream(Code::H3_REQUEST_CANCELLED);
        wait_for_probe(&mut session).await;
        assert!(!session.is_body_empty());
        assert!(session.read_body_bytes().await.is_err());
    }

    #[tokio::test]
    async fn h3_body_timeout_preserves_stream_for_later_fin() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        session.set_read_timeout(Some(Duration::from_millis(5)));
        assert!(session.read_body_bytes().await.is_err());
        assert!(!session.is_body_empty());
        client.finish().await.unwrap();
        session.set_read_timeout(Some(Duration::from_secs(2)));
        assert!(session.read_body_bytes().await.unwrap().is_none());
        assert!(session.is_body_empty());
    }

    #[tokio::test]
    async fn h3_retry_regression_trailers_keep_the_body_end_marker() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        session.enable_retry_buffering();
        let mut trailers = HeaderMap::new();
        trailers.insert("authorization", "trailer-only".parse().unwrap());
        client.send_trailers(trailers).await.unwrap();
        client.finish().await.unwrap();
        wait_for_probe(&mut session).await;
        assert!(session.is_body_done());
        assert!(!session.is_body_empty());
        // Pingora needs an initial pipe task when FIN was already consumed
        // and trailers prohibit the body-empty cache-admission shortcut.
        assert!(session.get_retry_buffer().unwrap().is_empty());
    }

    #[tokio::test]
    async fn h3_retry_regression_truncation_never_exposes_a_partial_replay() {
        let (mut session, mut client, _pair) = pair(get(None)).await;
        session.enable_retry_buffering();
        let prefix = Bytes::from(vec![0x5a; 64 * 1024]);
        client.send_data(prefix.clone()).await.unwrap();
        let mut received = Vec::new();
        while received.len() < prefix.len() {
            let chunk = session.read_body_bytes().await.unwrap().unwrap();
            received.extend_from_slice(&chunk);
        }
        assert_eq!(session.get_retry_buffer().unwrap(), prefix);
        assert!(!session.retry_buffer_truncated());
        client.send_data(Bytes::from_static(b"x")).await.unwrap();
        assert_eq!(
            session.read_body_bytes().await.unwrap().unwrap().as_ref(),
            b"x"
        );
        assert!(session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().is_none());
        assert!(session.retry_buffer.as_ref().unwrap().is_empty());
        client.send_data(Bytes::from_static(b"tail")).await.unwrap();
        client.finish().await.unwrap();
        while session.read_body_bytes().await.unwrap().is_some() {
            assert!(session.get_retry_buffer().is_none());
            assert!(session.retry_buffer.as_ref().unwrap().is_empty());
        }
        assert_eq!(session.body_bytes_read(), prefix.len() + 5);
        assert!(session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().is_none());
        session.enable_retry_buffering();
        assert!(!session.retry_buffer_truncated());
        assert!(session.get_retry_buffer().unwrap().is_empty());
    }

    #[test]
    fn h3_request_adapter_preserves_duplicate_headers() {
        let mut request = get(None);
        request
            .headers_mut()
            .append("x-repeat", "one".parse().unwrap());
        request
            .headers_mut()
            .append("x-repeat", "two".parse().unwrap());
        request
            .headers_mut()
            .append("cookie", "a=1".parse().unwrap());
        request
            .headers_mut()
            .append("cookie", "b=2".parse().unwrap());
        let header = build_request_header(request).unwrap();
        assert_eq!(header.headers.get_all("x-repeat").iter().count(), 2);
        assert_eq!(header.headers.get_all("cookie").iter().count(), 2);
        assert_eq!(header.headers[http::header::HOST], "openrusty.org");
    }
}
