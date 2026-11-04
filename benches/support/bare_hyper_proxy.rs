//! OFF-only framework control. Never a replacement production Edge server.
use anyhow::{Context, bail, ensure};
use http::uri::{Authority, Scheme};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, HeaderName};
use hyper::service::service_fn;
use hyper::{Request, Response, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use std::convert::Infallible;
use std::fs::File;
use std::io::BufReader;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

type OriginClient = Client<HttpConnector, Incoming>;
type ServiceError = Box<dyn std::error::Error + Send + Sync>;

fn remove_hop_headers(headers: &mut hyper::HeaderMap) -> anyhow::Result<()> {
    let nominated: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "te",
    ] {
        headers.remove(name);
    }
    Ok(())
}

async fn forward(
    mut request: Request<Incoming>,
    client: OriginClient,
    authority: Authority,
) -> Result<Response<Incoming>, ServiceError> {
    let mut parts = request.uri().clone().into_parts();
    parts.scheme = Some(Scheme::HTTP);
    parts.authority = Some(authority);
    *request.uri_mut() = Uri::from_parts(parts)?;
    *request.version_mut() = hyper::Version::HTTP_11;
    remove_hop_headers(request.headers_mut())?;
    let mut response = client.request(request).await?;
    remove_hop_headers(response.headers_mut())?;
    Ok(response)
}

async fn serve<S>(stream: S, client: OriginClient, authority: Authority)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| forward(request, client.clone(), authority.clone()));
    if let Err(error) = Builder::new(TokioExecutor::new())
        .serve_connection(TokioIo::new(stream), service)
        .await
    {
        eprintln!("bare Hyper connection failed: {error}");
    }
}

async fn accept(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    client: OriginClient,
    authority: Authority,
) -> anyhow::Result<Infallible> {
    loop {
        let (socket, _) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let (tls, client, authority) = (tls.clone(), client.clone(), authority.clone());
        tokio::spawn(async move {
            if let Some(tls) = tls {
                match tls.accept(socket).await {
                    Ok(stream) => serve(stream, client, authority).await,
                    Err(error) => eprintln!("bare Hyper TLS handshake failed: {error}"),
                }
            } else {
                serve::<TcpStream>(socket, client, authority).await;
            }
        });
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut origin, mut cert, mut key) = (None, None, None);
    let (mut threads, mut pool) = (1, 256);
    while let Some(arg) = args.next() {
        if arg == "--version" {
            println!(
                "bare-hyper 1.11.0 diagnostic; rustls AWS-LC; mimalloc; H1 H1-TLS H2; OFF only"
            );
            return Ok(());
        }
        let value = args
            .next()
            .with_context(|| format!("missing value for {arg}"))?;
        match arg.as_str() {
            "--origin" => origin = Some(value),
            "--cert" => cert = Some(value),
            "--key" => key = Some(value),
            "--threads" => threads = value.parse()?,
            "--pool" => pool = value.parse()?,
            _ => bail!("unknown argument {arg}"),
        }
    }
    ensure!(threads > 0 && pool > 0, "threads and pool must be nonzero");
    let origin = origin
        .context("--origin required")?
        .to_socket_addrs()?
        .next()
        .context("origin resolved to no address")?;
    let authority = origin.to_string().parse()?;
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("TLS provider already installed"))?;
    let certificates = rustls_pemfile::certs(&mut BufReader::new(File::open(
        cert.context("--cert required")?,
    )?))
    .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(File::open(
        key.context("--key required")?,
    )?))?
    .context("no private key in fixture file")?;
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)?;
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tls = TlsAcceptor::from(Arc::new(tls));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        connector.set_connect_timeout(Some(Duration::from_secs(15)));
        connector.set_keepalive(Some(Duration::from_secs(60)));
        let client = Client::builder(TokioExecutor::new())
            .pool_max_idle_per_host(pool)
            .pool_idle_timeout(Duration::from_secs(120))
            .build(connector);
        let plain = TcpListener::bind("0.0.0.0:19080").await?;
        let secure = TcpListener::bind("0.0.0.0:19443").await?;
        let first = tokio::select! {
            result = accept(plain, None, client.clone(), authority.clone()) => result,
            result = accept(secure, Some(tls), client, authority) => result,
        };
        match first {
            Ok(never) => match never {},
            Err(error) => Err(error),
        }
    })
}
