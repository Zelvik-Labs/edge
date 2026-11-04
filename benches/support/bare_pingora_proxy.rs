//! Diagnostic OFF-only framework control, never a replacement Edge runtime.
use anyhow::{Context, bail, ensure};
use async_trait::async_trait;
use pingora::proxy::{ProxyHttp, Session};
use pingora::server::Server;
use pingora::server::configuration::ServerConf;
use pingora::upstreams::peer::HttpPeer;
use std::net::ToSocketAddrs;
use std::time::Duration;

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct BareProxy {
    peer: HttpPeer,
}

#[async_trait]
impl ProxyHttp for BareProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut (),
    ) -> pingora::Result<Box<HttpPeer>> {
        Ok(Box::new(self.peer.clone()))
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let (mut origin, mut cert, mut key) = (None, None, None);
    let (mut threads, mut pool) = (1, 256);
    while let Some(arg) = args.next() {
        if arg == "--version" {
            println!(
                "bare-pingora 0.9.0 diagnostic; rustls AWS-LC; mimalloc; H1 H1-TLS H2; OFF only"
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
    let address = origin
        .context("--origin is required")?
        .to_socket_addrs()?
        .next()
        .context("origin resolved to no address")?;
    let cert = cert.context("--cert is required")?;
    let key = key.context("--key is required")?;
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("TLS provider was already installed before startup"))?;
    let config = ServerConf {
        threads,
        upstream_keepalive_pool_size: pool,
        grace_period_seconds: Some(5),
        graceful_shutdown_timeout_seconds: Some(5),
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(None, config);
    server.bootstrap();
    let mut peer = HttpPeer::new(address, false, String::new());
    peer.options.connection_timeout = Some(Duration::from_secs(15));
    peer.options.idle_timeout = Some(Duration::from_secs(120));
    peer.options.tcp_keepalive = Some(pingora::protocols::l4::ext::TcpKeepalive {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(10),
        count: 3,
        #[cfg(target_os = "linux")]
        user_timeout: Duration::ZERO,
    });
    let mut service = pingora::proxy::http_proxy_service(&server.configuration, BareProxy { peer });
    service.add_tcp("0.0.0.0:19080");
    let mut tls = pingora::listeners::tls::TlsSettings::intermediate(&cert, &key)?;
    tls.enable_h2();
    service.add_tls_with_settings("0.0.0.0:19443", None, tls);
    server.add_service(service);
    server.run_forever();
}
