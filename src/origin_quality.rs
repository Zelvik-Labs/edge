//! Active, hop-local HEAD quality measurements for compiled smart origin paths.
//! These observations never change business health or origin availability.
use crate::lb_factory::BackendExtension;
use crate::origin_route::OriginPathObserver;
use pingora_load_balancing::{
    Backend,
    health_check::{HealthCheck, HttpHealthCheck},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

pub(crate) struct ActiveOriginPath {
    pub observer: Arc<OriginPathObserver>,
    in_flight: AtomicBool,
    cursor: AtomicU64,
    next_at: AtomicU64,
}
impl ActiveOriginPath {
    fn new(observer: Arc<OriginPathObserver>) -> Self {
        Self {
            observer,
            in_flight: AtomicBool::new(false),
            cursor: AtomicU64::new(0),
            next_at: AtomicU64::new(0),
        }
    }
    pub fn shared(observer: Arc<OriginPathObserver>) -> Arc<Self> {
        type Registry = std::collections::HashMap<usize, std::sync::Weak<ActiveOriginPath>>;
        static PATHS: std::sync::LazyLock<parking_lot::Mutex<Registry>> =
            std::sync::LazyLock::new(|| parking_lot::Mutex::new(Registry::new()));
        let key = Arc::as_ptr(&observer) as usize;
        let mut paths = PATHS.lock();
        if let Some(active) = paths.get(&key).and_then(std::sync::Weak::upgrade) {
            return active;
        }
        if paths.len().is_multiple_of(256) {
            paths.retain(|_, path| path.strong_count() > 0);
        }
        let active = Arc::new(Self::new(observer));
        paths.insert(key, Arc::downgrade(&active));
        active
    }

    pub fn reserve_at(self: &Arc<Self>, now: u64) -> Option<ActiveOriginPermit> {
        self.in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        let permit = ActiveOriginPermit(Arc::clone(self));
        let due = self.next_at.load(Ordering::Relaxed);
        if now < due
            || self
                .next_at
                .compare_exchange(
                    due,
                    now.saturating_add(5),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return None;
        }
        Some(permit)
    }
    pub fn next_backend(&self, count: usize) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed) as usize % count
    }
}
pub(crate) struct ActiveOriginPermit(Arc<ActiveOriginPath>);
impl Drop for ActiveOriginPermit {
    fn drop(&mut self) {
        self.0.in_flight.store(false, Ordering::Release)
    }
}
pub(crate) struct OriginQualityJob {
    pub path_id: String,
    pub backend: Backend,
    pub active: Arc<ActiveOriginPath>,
    pub valid_until: u64,
    pub _permit: ActiveOriginPermit,
    pub path: String,
}

pub(crate) fn build_head_check(
    backend: &Backend,
    default_host: &str,
    path: &str,
) -> Result<HttpHealthCheck, String> {
    let ext = backend
        .ext
        .get::<BackendExtension>()
        .ok_or("origin quality requires compiled endpoint metadata")?;
    if ext.unsupported_reason.is_some() {
        return Err("origin quality endpoint is not executable".into());
    }
    if ext.origin_authentication.is_some() && !ext.use_tls {
        return Err("origin quality authentication requires TLS".into());
    }
    // A source that requires a client identity or visitor-dependent transport
    // must not be represented by a successful probe using different semantics.
    if ext.client_cert.is_some() || ext.proxy_protocol.enabled() || ext.follow_port {
        return Err("origin quality requires a bound client identity and a fixed transport".into());
    }
    let host = if !ext.host.is_empty() {
        &ext.host
    } else if !ext.rp_host.is_empty() {
        &ext.rp_host
    } else {
        default_host
    };
    if host.is_empty() || host.contains('*') {
        return Err("origin quality requires a concrete HTTP Host".into());
    }
    let host = if ext.request_host_excluding_port {
        crate::lb_factory::strip_addr_port(host)
    } else {
        host.to_string()
    };
    let sni = if !ext.tls_server_name.is_empty() {
        ext.tls_server_name.clone()
    } else {
        crate::lb_factory::strip_addr_port(&host)
    };
    let mut check = HttpHealthCheck::new(&host, ext.use_tls);
    check.peer_template.sni = sni;
    check.peer_template.options.verify_cert = true;
    check.peer_template.options.verify_hostname = true;
    check.peer_template.options.connection_timeout =
        Some(ext.connection_timeout.unwrap_or(Duration::from_secs(15)));
    check.peer_template.options.read_timeout =
        Some(ext.read_timeout.unwrap_or(Duration::from_secs(15)));
    check.peer_template.options.write_timeout =
        Some(ext.write_timeout.unwrap_or(Duration::from_secs(15)));
    if ext.http2_enabled && ext.use_tls {
        check.peer_template.options.alpn = pingora_core::protocols::ALPN::H2H1
    }
    check.req = pingora_http::RequestHeader::build("HEAD", path.as_bytes(), None)
        .map_err(|_| "invalid origin quality path")?;
    check
        .req
        .insert_header("Host", host)
        .map_err(|_| "invalid origin quality Host")?;
    check
        .req
        .insert_header("Cache-Control", "no-cache, no-store")
        .map_err(|_| "invalid origin quality cache policy")?;
    check
        .req
        .insert_header("Connection", "close")
        .map_err(|_| "invalid origin quality connection policy")?;
    if let Some(auth) = &ext.origin_authentication {
        check
            .req
            .insert_header(crate::origin_route::AUTH_HEADER, auth.token.as_str())
            .map_err(|_| "invalid origin quality authentication")?;
        check
            .req
            .insert_header(
                crate::origin_route::DEPLOYMENT_HEADER,
                auth.deployment_id.as_str(),
            )
            .map_err(|_| "invalid origin quality identity")?;
        check
            .req
            .insert_header(crate::origin_route::HOP_HEADER, "1")
            .map_err(|_| "invalid origin quality hop")?;
    }
    check.reuse_connection = false;
    check.validator = Some(Box::new(|response| {
        if response.status.is_success() || response.status.is_redirection() {
            Ok(())
        } else {
            pingora_core::Error::e_explain(
                pingora_core::ErrorType::CustomCode(
                    "origin HEAD quality status",
                    response.status.as_u16(),
                ),
                "HEAD quality is unavailable; business health is unchanged",
            )
        }
    }));
    Ok(check)
}

pub(crate) async fn execute_quality_job(job: OriginQualityJob, default_host: &str) {
    let check = match build_head_check(&job.backend, default_host, &job.path) {
        Ok(check) => check,
        Err(reason) => {
            tracing::warn!(path_id=%job.path_id,%reason,"origin quality probe configuration rejected");
            return;
        }
    };
    run_head_check(job, check).await;
}

async fn run_head_check(job: OriginQualityJob, mut check: HttpHealthCheck) {
    let now = crate::origin_route::unix_now();
    if job.valid_until <= now {
        return;
    }
    let started = Instant::now();
    let observed = Arc::new(AtomicBool::new(false));
    let observed_header = Arc::clone(&observed);
    let observer = Arc::clone(&job.active.observer);
    // The callback runs as soon as response headers arrive, before body drain.
    // Its duration includes this hop's connect/TLS/request, never visitor work.
    check.validator = Some(Box::new(move |response| {
        let success = response.status.is_success() || response.status.is_redirection();
        observer.record(started.elapsed(), success);
        observed_header.store(true, Ordering::Release);
        if success {
            Ok(())
        } else {
            pingora_core::Error::e_explain(
                pingora_core::ErrorType::CustomCode(
                    "origin HEAD quality status",
                    response.status.as_u16(),
                ),
                "HEAD quality is unavailable; business health is unchanged",
            )
        }
    }));
    let remaining = Duration::from_secs(job.valid_until - now);
    match tokio::time::timeout(remaining, check.check(&job.backend)).await {
        Ok(result) => {
            if result.is_err() && !observed.load(Ordering::Acquire) {
                job.active.observer.record(started.elapsed(), false)
            }
            if let Err(error) = result {
                tracing::debug!(path_id=%job.path_id,%error,"active origin quality failed; business health unchanged")
            }
        }
        Err(_) => tracing::debug!(path_id=%job.path_id,"origin quality lease expired during probe"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lb_factory::{build_lb, peer_origin_id};
    use crate::origin_route::unix_now;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const CERT: &[u8] =
        include_bytes!("../pingora-main/pingora-proxy/tests/utils/conf/keys/server_rustls.crt");
    const KEY: &[u8] =
        include_bytes!("../pingora-main/pingora-proxy/tests/utils/conf/keys/key.pem");

    fn pool(addresses: &[std::net::SocketAddr]) -> Arc<crate::lb_factory::AnyLoadBalancer> {
        let now = unix_now();
        let paths:Vec<_>=addresses.iter().enumerate().map(|(i,address)|serde_json::json!({
            "id":format!("path-{i}"),"kind":"shield","priority":i+1,"ready":true,"validUntil":now+120,
            "origins":[{"id":820000+i,"weight":1,"addr":format!("https://{address}"),
                "requestHost":"content.example","tlsServerName":"probe.openrusty.org",
                "originAuthentication":{"deploymentId":"active-quality-deployment","token":"0123456789abcdef0123456789abcdef"}}]
        })).collect();
        let config=serde_json::from_value(serde_json::json!({"isOn":true,"originRoute":{
            "siteId":"active-quality-site","deploymentId":"active-quality-deployment","revision":1,
            "mode":"smart_shield","role":"ingress","validUntil":now+120,"cacheNamespace":"active-quality",
            "paths":paths
        }})).unwrap();
        build_lb(820000, &config, 1, &HashMap::new(), false, true).0
    }

    async fn tls_origin(status: u16) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            crate::tls_crypto::default_crypto_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            crate::ssl::parse_cert_chain(CERT).unwrap(),
            crate::ssl::parse_private_key(KEY).unwrap(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = acceptor.accept(stream).await.unwrap();
            assert_eq!(
                stream.get_ref().1.server_name(),
                Some("probe.openrusty.org")
            );
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 1024];
                let n = stream.read(&mut bytes).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&bytes[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(
                request.starts_with("head /probe?quality=1 http/1.1\r\n"),
                "{request}"
            );
            assert!(request.contains("host: content.example\r\n"));
            assert!(
                request.contains("x-zelvik-origin-token: 0123456789abcdef0123456789abcdef\r\n")
            );
            assert!(request.contains("x-zelvik-origin-deployment: active-quality-deployment\r\n"));
            assert!(request.contains("x-zelvik-origin-hop: 1\r\n"));
            stream.write_all(format!("HTTP/1.1 {status} quality\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        (address, task)
    }

    fn trust_fixture(check: &mut HttpHealthCheck) {
        use pingora_core::utils::tls::{WrappedX509, parse_x509};
        let certs = crate::ssl::parse_cert_chain(CERT).unwrap();
        let ca: Vec<_> = certs
            .into_iter()
            .map(|cert| WrappedX509::new(cert.as_ref().to_vec(), parse_x509))
            .collect();
        check.peer_template.options.ca = Some(Arc::from(ca.into_boxed_slice()));
    }

    #[tokio::test]
    async fn active_origin_samples_every_path_with_real_tls_host_sni_auth_and_keeps_head_failure_out_of_health()
     {
        let (first, first_task) = tls_origin(200).await;
        let (backup, backup_task) = tls_origin(405).await;
        let pool = pool(&[first, backup]);
        let now = unix_now();
        let mut jobs = pool.take_active_origin_probes(now);
        assert_eq!(
            jobs.len(),
            2,
            "backup must be sampled without visitor traffic"
        );
        assert!(pool.take_active_origin_probes(now + 1).is_empty());
        assert!(
            pool.take_active_origin_probes(now + 5).is_empty(),
            "in-flight reservation is bounded"
        );
        for mut job in jobs.drain(..) {
            job.path = "/probe?quality=1".into();
            let mut check = build_head_check(&job.backend, "unused.example", &job.path).unwrap();
            assert!(check.peer_template.options.verify_cert);
            assert!(check.peer_template.options.verify_hostname);
            trust_fixture(&mut check);
            tokio::time::timeout(Duration::from_secs(5), run_head_check(job, check))
                .await
                .unwrap();
        }
        first_task.await.unwrap();
        backup_task.await.unwrap();
        let observations = pool.origin_path_observations(820000, unix_now());
        assert_eq!(observations.len(), 2);
        assert!(
            observations
                .iter()
                .all(|o| o["sourceKind"] == "active_origin" && o["windowSeconds"] == 90)
        );
        assert_eq!(observations[0]["successCount"], 1);
        assert_eq!(observations[1]["errorCount"], 1);
        assert!(
            observations[1]["ttfbMicros"].is_null(),
            "HEAD405 is unavailable quality"
        );
        assert!(
            pool.backend_health().iter().all(|(_, healthy)| *healthy),
            "quality failures cannot change business health"
        );
        assert_eq!(
            peer_origin_id(&pool.select(b"ordinary-get", 32).unwrap()),
            820000
        );
        assert_eq!(
            pool.take_active_origin_probes(now + 10).len(),
            2,
            "permit released after completion"
        );
    }

    #[tokio::test]
    async fn active_origin_expired_job_performs_no_request_and_reports_no_fabricated_sample() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pool = pool(&[listener.local_addr().unwrap()]);
        let mut job = pool.take_active_origin_probes(unix_now()).pop().unwrap();
        job.valid_until = unix_now();
        execute_quality_job(job, "content.example").await;
        assert!(pool.origin_path_observations(820000, unix_now()).is_empty());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn active_origin_default_transport_verifies_tls_and_never_sends_auth_to_plaintext() {
        let pool = pool(&["127.0.0.1:8443".parse().unwrap()]);
        let job = pool.take_active_origin_probes(unix_now()).pop().unwrap();
        let check = build_head_check(&job.backend, "content.example", "/").unwrap();
        assert!(check.peer_template.options.verify_cert);
        assert!(check.peer_template.options.verify_hostname);
        assert!(
            check.peer_template.options.ca.is_none(),
            "runtime uses system trust"
        );
        assert_eq!(check.peer_template.sni, "probe.openrusty.org");
        assert!(check.health_changed_callback.is_none());
        assert!(!check.reuse_connection);
        let mut backend = job.backend.clone();
        backend.ext.get_mut::<BackendExtension>().unwrap().use_tls = false;
        assert!(build_head_check(&backend, "content.example", "/").is_err());
    }
}
