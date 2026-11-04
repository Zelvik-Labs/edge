//! Diagnostic H1 baseline: the production Pingora dependency, allocator and
//! Tokio listener/session loop, without Edge filters, accounting or caching.
//! This is not a production node and must never be reported as Edge capacity.
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use pingora_core::apps::HttpServerApp;
use pingora_core::protocols::http::server::Session as ServerSession;
use pingora_core::protocols::l4::stream::Stream;
use pingora_core::server::configuration::ServerConf;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_proxy::{ProxyHttp, Session, http_proxy};
use tokio::net::TcpListener;
use tokio::sync::watch;

#[global_allocator]
static GLOBAL_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
struct Options {
    #[arg(long, default_value = "0.0.0.0:19080")]
    listen: SocketAddr,
    #[arg(long)]
    origin: String,
}

struct MinimalProxy {
    peer: HttpPeer,
}

#[async_trait]
impl ProxyHttp for MinimalProxy {
    type CTX = ();

    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora_core::Result<Box<HttpPeer>> {
        Ok(Box::new(self.peer.clone()))
    }
}

fn main() -> Result<()> {
    let options = Options::parse();
    let origin = options
        .origin
        .to_socket_addrs()?
        .next()
        .context("origin has no address")?;
    zelvik_edge::memory_reclaim::configure_allocator();
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(2).enable_all();
    zelvik_edge::worker_reclaim::install_tokio_worker_hooks(&mut builder);
    builder.build()?.block_on(async {
        let conf = Arc::new(ServerConf {
            threads: 2,
            upstream_keepalive_pool_size: zelvik_edge::memory_governor::MEMORY_GOVERNOR
                .pingora_keepalive_pool_size(2),
            ..Default::default()
        });
        let mut peer = HttpPeer::new(origin, false, String::new());
        peer.options.connection_timeout = Some(Duration::from_secs(15));
        peer.options.idle_timeout = Some(Duration::from_secs(120));
        peer.options.tcp_keepalive = Some(pingora_core::protocols::l4::ext::TcpKeepalive {
            idle: Duration::from_secs(60),
            interval: Duration::from_secs(10),
            count: 3,
            #[cfg(target_os = "linux")]
            user_timeout: Duration::ZERO,
        });
        let proxy = Arc::new(http_proxy(&conf, MinimalProxy { peer }));
        let listener = TcpListener::bind(options.listen).await?;
        let (_shutdown, shutdown_rx) = watch::channel(false);
        eprintln!(
            "minimal H1 diagnostic ready: {} -> {origin}",
            listener.local_addr()?
        );
        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let proxy = Arc::clone(&proxy);
            let shutdown_rx = shutdown_rx.clone();
            tokio::spawn(async move {
                let mut session = ServerSession::new_http1(Box::new(Stream::from(stream)));
                session.set_read_timeout(Some(Duration::from_secs(60)));
                session.set_keepalive(Some(60));
                loop {
                    let Some((stream, settings)) = proxy
                        .process_new_http(session, &shutdown_rx)
                        .await
                        .map(|result| result.consume())
                    else {
                        break;
                    };
                    session = ServerSession::new_http1(stream);
                    if let Some(settings) = settings {
                        settings.apply_to_session(&mut session);
                    }
                    session.set_read_timeout(Some(Duration::from_secs(60)));
                }
            });
        }
    })
}
