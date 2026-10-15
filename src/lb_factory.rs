use crate::config_models::{
    GlobalHTTPAllConfig, HealthCheckConfig, OriginConfig, OriginTlsSecurityVerifyMode,
    ParentNodeConfig, ProxyProtocolConfig, ReverseProxyConfig,
};
use futures_util::FutureExt;
use http;
use http::Extensions;
use pingora_load_balancing::{
    Backend, Backends, LoadBalancer,
    discovery::Static,
    health_check,
    selection::{BackendIter, BackendSelection, Consistent, Random, RoundRobin},
};
use std::collections::{BTreeSet, HashMap};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

#[derive(Clone)]
pub enum AnyLoadBalancer {
    RoundRobin(Arc<LoadBalancer<RoundRobin>>),
    Random(Arc<LoadBalancer<Random>>),
    /// Ketama consistent hashing (`scheduling.code == "hash"`); keys on the
    /// client IP so a given client sticks to the same origin.
    Consistent(Arc<LoadBalancer<Consistent>>),
    /// Least-connections (`scheduling.code == "least_conn"`); ranks candidates by
    /// the live per-origin in-flight count held in [`crate::origin_state`].
    LeastConn(Arc<LoadBalancer<LeastConn>>),
    OriginPool {
        primary: Arc<AnyLoadBalancer>,
        backup: Option<Arc<AnyLoadBalancer>>,
    },
}

/// Least-connections selection algorithm.
///
/// Pingora ships no least-connections implementation (`selection/mod.rs` has a
/// `// TODO: least conn`). This type implements its [`BackendSelection`] trait:
/// [`Self::iter`] yields every backend ordered by the current per-origin
/// in-flight count (ascending), so the first candidate the caller accepts is
/// the least-loaded one.
pub struct LeastConn {
    backends: Box<[Backend]>,
    counters: Arc<crate::origin_state::OriginInflightCounters>,
}

impl BackendSelection for LeastConn {
    type Iter = LeastConnIterator;
    type Config = Arc<crate::origin_state::OriginInflightCounters>;

    fn build(backends: &BTreeSet<Backend>) -> Self {
        Self {
            backends: backends.iter().cloned().collect(),
            counters: Arc::new(crate::origin_state::OriginInflightCounters::new()),
        }
    }

    fn build_with_config(backends: &BTreeSet<Backend>, config: &Self::Config) -> Self {
        Self {
            backends: backends.iter().cloned().collect(),
            counters: Arc::clone(config),
        }
    }

    fn iter(self: &Arc<Self>, _key: &[u8]) -> Self::Iter {
        // Snapshot the candidate order at selection time by the live in-flight
        // count; ties keep the deterministic backend (BTreeSet) order.
        let mut ordered: Vec<Backend> = self.backends.to_vec();
        ordered.sort_by_key(|backend| self.counters.inflight(peer_origin_id(backend)));
        LeastConnIterator { ordered, index: 0 }
    }
}

/// Iterator over [`LeastConn`] candidates, pre-ordered by ascending in-flight
/// count at the moment of selection.
pub struct LeastConnIterator {
    ordered: Vec<Backend>,
    index: usize,
}

impl BackendIter for LeastConnIterator {
    fn next(&mut self) -> Option<&Backend> {
        let backend = self.ordered.get(self.index);
        self.index += 1;
        backend
    }
}

impl AnyLoadBalancer {
    pub fn select(&self, key: &[u8], max_iterations: usize) -> Option<Backend> {
        match self {
            Self::RoundRobin(lb) => lb.select(key, max_iterations),
            Self::Random(lb) => lb.select(key, max_iterations),
            Self::Consistent(lb) => {
                let mut buf = [0u8; 8];
                lb.select(consistent_key(key, &mut buf), max_iterations)
            }
            Self::LeastConn(lb) => lb.select(key, max_iterations),
            Self::OriginPool { primary, .. } => primary.select(key, max_iterations),
        }
    }

    /// Whether this pool's consistent-hash algorithm keys on the client IP, so
    /// the caller must pass the client IP bytes as the selection key.
    pub fn uses_client_ip_key(&self) -> bool {
        match self {
            Self::Consistent(_) => true,
            Self::OriginPool { primary, .. } => primary.uses_client_ip_key(),
            _ => false,
        }
    }

    /// Whether least-connections drives selection, so the caller must maintain
    /// the per-origin in-flight count for the selected backend.
    pub fn is_least_conn(&self) -> bool {
        match self {
            Self::LeastConn(_) => true,
            Self::OriginPool { primary, .. } => primary.is_least_conn(),
            _ => false,
        }
    }

    pub fn select_with_backup<F>(
        &self,
        key: &[u8],
        max_iterations: usize,
        is_down: F,
    ) -> Option<Backend>
    where
        F: Fn(i64) -> bool + Copy,
    {
        match self {
            Self::OriginPool { primary, backup } => {
                if let Some(peer) = primary.select_healthy(key, max_iterations, is_down) {
                    return Some(peer);
                }
                backup
                    .as_ref()
                    .and_then(|lb| lb.select_healthy(key, max_iterations, is_down))
                    .or_else(|| primary.select_down_fallback(key, max_iterations, is_down))
                    .or_else(|| {
                        backup
                            .as_ref()
                            .and_then(|lb| lb.select_down_fallback(key, max_iterations, is_down))
                    })
                    .or_else(|| primary.select(key, max_iterations))
            }
            _ => self.select_available(key, max_iterations, is_down),
        }
    }

    fn select_available<F>(&self, key: &[u8], max_iterations: usize, is_down: F) -> Option<Backend>
    where
        F: Fn(i64) -> bool + Copy,
    {
        self.select_healthy(key, max_iterations, is_down)
            .or_else(|| self.select_down_fallback(key, max_iterations, is_down))
    }

    /// First healthy backend that is not marked down by the caller.
    ///
    /// Uses [`LoadBalancer::select_with`] so that the underlying
    /// [`BackendIter`] actually advances (along the Ketama ring for consistent
    /// hashing, along the weighted list otherwise). Re-issuing `select` with the
    /// same key would return the same backend forever for a deterministic
    /// algorithm and spin until `max_iterations` was exhausted.
    fn select_healthy<F>(&self, key: &[u8], max_iterations: usize, is_down: F) -> Option<Backend>
    where
        F: Fn(i64) -> bool + Copy,
    {
        let accept = move |peer: &Backend, health: bool| {
            health && !(peer_origin_id(peer) > 0 && is_down(peer_origin_id(peer)))
        };
        match self {
            Self::RoundRobin(lb) => lb.select_with(key, max_iterations, accept),
            Self::Random(lb) => lb.select_with(key, max_iterations, accept),
            Self::Consistent(lb) => {
                let mut buf = [0u8; 8];
                lb.select_with(consistent_key(key, &mut buf), max_iterations, accept)
            }
            Self::LeastConn(lb) => lb.select_with(key, max_iterations, accept),
            Self::OriginPool { primary, .. } => primary.select_healthy(key, max_iterations, is_down),
        }
    }

    /// Fallback: first backend that is healthy by health-check but marked down
    /// by the caller's failure tracker.
    fn select_down_fallback<F>(
        &self,
        key: &[u8],
        max_iterations: usize,
        is_down: F,
    ) -> Option<Backend>
    where
        F: Fn(i64) -> bool + Copy,
    {
        let accept = move |peer: &Backend, health: bool| {
            health && peer_origin_id(peer) > 0 && is_down(peer_origin_id(peer))
        };
        match self {
            Self::RoundRobin(lb) => lb.select_with(key, max_iterations, accept),
            Self::Random(lb) => lb.select_with(key, max_iterations, accept),
            Self::Consistent(lb) => {
                let mut buf = [0u8; 8];
                lb.select_with(consistent_key(key, &mut buf), max_iterations, accept)
            }
            Self::LeastConn(lb) => lb.select_with(key, max_iterations, accept),
            Self::OriginPool { primary, .. } => {
                primary.select_down_fallback(key, max_iterations, is_down)
            }
        }
    }

    pub fn run_health_check(
        &self,
        parallel: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match self {
                Self::RoundRobin(lb) => lb.backends().run_health_check(parallel).await,
                Self::Random(lb) => lb.backends().run_health_check(parallel).await,
                Self::Consistent(lb) => lb.backends().run_health_check(parallel).await,
                Self::LeastConn(lb) => lb.backends().run_health_check(parallel).await,
                Self::OriginPool { primary, backup } => {
                    primary.run_health_check(parallel).await;
                    if let Some(backup) = backup {
                        backup.run_health_check(parallel).await;
                    }
                }
            }
        })
    }

    pub fn backend_health(&self) -> Vec<(Backend, bool)> {
        match self {
            Self::RoundRobin(lb) => lb
                .backends()
                .get_backend()
                .iter()
                .map(|backend| (backend.clone(), lb.backends().ready(backend)))
                .collect(),
            Self::Random(lb) => lb
                .backends()
                .get_backend()
                .iter()
                .map(|backend| (backend.clone(), lb.backends().ready(backend)))
                .collect(),
            Self::Consistent(lb) => lb
                .backends()
                .get_backend()
                .iter()
                .map(|backend| (backend.clone(), lb.backends().ready(backend)))
                .collect(),
            Self::LeastConn(lb) => lb
                .backends()
                .get_backend()
                .iter()
                .map(|backend| (backend.clone(), lb.backends().ready(backend)))
                .collect(),
            Self::OriginPool { primary, backup } => {
                let mut health = primary.backend_health();
                if let Some(backup) = backup {
                    health.extend(backup.backend_health());
                }
                health
            }
        }
    }

    pub fn as_round_robin(&self) -> Option<Arc<LoadBalancer<RoundRobin>>> {
        match self {
            Self::RoundRobin(lb) => Some(Arc::clone(lb)),
            Self::Random(_) => None,
            // Deterministic / connection-aware algorithms are not round robin.
            Self::Consistent(_) => None,
            Self::LeastConn(_) => None,
            Self::OriginPool { primary, .. } => primary.as_round_robin(),
        }
    }
}

pub fn peer_origin_id(peer: &Backend) -> i64 {
    peer.ext
        .get::<BackendExtension>()
        .map(|ext| ext.origin_id)
        .unwrap_or(0)
}

/// Key used for consistent-hash selection.
///
/// A non-empty key (the L7 path passes the client IP) is used as-is. An empty
/// key — the L4 transports pass `b""` — would otherwise pin every request to a
/// single ring position, so substitute random bytes; this is the same fallback
/// the parent L2 pool uses at `proxy.rs` (random key when no mapping applies).
fn consistent_key<'a>(key: &'a [u8], buf: &'a mut [u8; 8]) -> &'a [u8] {
    if key.is_empty() {
        buf.copy_from_slice(&rand::random::<u64>().to_le_bytes());
        &buf[..]
    } else {
        key
    }
}

pub fn should_verify_origin_tls(
    ext: &BackendExtension,
    sni_host: &str,
    client_host: Option<&str>,
) -> bool {
    if let Some(legacy) = ext.legacy_tls_verify {
        return legacy;
    }

    match ext.tls_security_verify_mode {
        OriginTlsSecurityVerifyMode::Force => true,
        OriginTlsSecurityVerifyMode::Skip => false,
        OriginTlsSecurityVerifyMode::Auto => {
            let sni_host = strip_addr_port(sni_host).to_ascii_lowercase();
            if sni_host.is_empty() {
                return true;
            }
            let origin_host = &ext.origin_host_normalized;

            if !origin_host.is_empty() && sni_host == *origin_host {
                return true;
            }
            if ext.explicit_tls_host_normalized.as_deref() == Some(sni_host.as_str()) {
                return true;
            }

            client_host
                .map(strip_addr_port)
                .map(|host| host.to_ascii_lowercase() != sni_host)
                .unwrap_or(false)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OriginRole {
    Primary,
    Backup,
    Fallback,
}

/// Custom metadata stored in Backend Extensions
#[derive(Clone, Debug)]
pub struct BackendExtension {
    pub origin_role: OriginRole,
    pub use_tls: bool,
    pub host: String,    // Per-origin custom host (origin.requestHost)
    pub rp_host: String, // Reverse-proxy-level custom host (reverseProxy.requestHost)
    pub origin_id: i64,
    pub origin_host: String,
    pub origin_host_normalized: String,
    pub explicit_tls_host_normalized: Option<String>,
    pub follow_port: bool,
    pub follow_host: bool,
    pub http2_enabled: bool,
    pub http3_enabled: bool,
    pub tls_security_verify_mode: OriginTlsSecurityVerifyMode,
    pub legacy_tls_verify: Option<bool>,
    pub request_host_excluding_port: bool,
    pub proxy_protocol: ProxyProtocolConfig,
    pub connection_timeout: Option<Duration>,
    pub read_timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub write_timeout: Option<Duration>,
    pub client_cert: Option<crate::config_models::SSLCertConfig>,
    pub unsupported_reason: Option<String>,
    pub oss_backend: Option<crate::oss_origin::OssBackend>,
}

pub fn build_lb(
    _server_id: i64,
    rp_cfg: &ReverseProxyConfig,
    _level: i32,
    _parent_nodes: &HashMap<i64, Vec<ParentNodeConfig>>,
    _tiered_origin_bypass: bool,
    allow_lan: bool,
) -> (Arc<AnyLoadBalancer>, bool) {
    build_origin_pool(rp_cfg, allow_lan, None)
}

pub fn build_lb_with_global_http(
    _server_id: i64,
    rp_cfg: &ReverseProxyConfig,
    _level: i32,
    _parent_nodes: &HashMap<i64, Vec<ParentNodeConfig>>,
    _tiered_origin_bypass: bool,
    allow_lan: bool,
    global_http: Option<&GlobalHTTPAllConfig>,
) -> (Arc<AnyLoadBalancer>, bool) {
    build_origin_pool(rp_cfg, allow_lan, global_http)
}

fn build_origin_pool(
    rp_cfg: &ReverseProxyConfig,
    allow_lan: bool,
    global_http: Option<&GlobalHTTPAllConfig>,
) -> (Arc<AnyLoadBalancer>, bool) {
    let (primary, primary_hc) = build_origin_lb(
        rp_cfg,
        &rp_cfg.primary_origins,
        OriginRole::Primary,
        allow_lan,
        global_http,
    );
    let (backup, backup_hc) = build_origin_lb(
        rp_cfg,
        &rp_cfg.backup_origins,
        OriginRole::Backup,
        allow_lan,
        global_http,
    );

    match (primary, backup) {
        (Some(primary), backup) => (
            Arc::new(AnyLoadBalancer::OriginPool { primary, backup }),
            primary_hc || backup_hc,
        ),
        (None, Some(backup)) => (backup, backup_hc),
        (None, None) => fallback_lb(),
    }
}

fn build_origin_lb(
    rp_cfg: &ReverseProxyConfig,
    origins: &[OriginConfig],
    role: OriginRole,
    allow_lan: bool,
    global_http: Option<&GlobalHTTPAllConfig>,
) -> (Option<Arc<AnyLoadBalancer>>, bool) {
    let mut endpoints = Vec::new();
    let mut unsupported_endpoints = Vec::new();
    let mut detected_hc = None;

    for origin in origins {
        if !origin.is_on {
            continue;
        }

        if origin.is_oss() {
            match oss_origin_backend(origin, role, rp_cfg, allow_lan, global_http) {
                Some(backend) => endpoints.push(backend),
                None => {
                    if let Some(backend) = unsupported_origin_backend(
                        origin,
                        role,
                        "OSS origin configuration is invalid",
                    ) {
                        unsupported_endpoints.push(backend);
                    }
                }
            }
            continue;
        }

        let Some(addr) = &origin.addr else {
            continue;
        };
        let target = addr.to_address();
        if !allow_lan && is_local_addr(&target) {
            warn!(
                "LB Builder: Skipping {:?} origin {} as it is not allowed in cluster settings.",
                role, target
            );
            continue;
        }
        let Some(mut backend) = backend_from_origin_target(&target, addr, "origin", allow_lan)
        else {
            continue;
        };

        let rp_host = reverse_proxy_request_host(rp_cfg, addr);
        let origin_host = addr.host();
        let explicit_tls_host_normalized = explicit_tls_host(&origin.request_host, &rp_host);
        let legacy_tls_verify = parse_legacy_tls_verify(origin.tls_verify.as_ref());
        let mut ext = Extensions::new();
        ext.insert(BackendExtension {
            origin_role: role,
            use_tls: addr.is_https(),
            host: origin.request_host.clone(),
            rp_host,
            origin_id: origin.id,
            origin_host: origin_host.clone(),
            origin_host_normalized: normalized_tls_host(&origin_host),
            explicit_tls_host_normalized,
            follow_port: origin.follow_port,
            follow_host: origin.follow_host,
            http2_enabled: origin.http2_enabled,
            http3_enabled: origin.http3_enabled,
            tls_security_verify_mode: origin.tls_security_verify_mode,
            legacy_tls_verify,
            request_host_excluding_port: rp_cfg.request_host_excluding_port,
            proxy_protocol: rp_cfg.proxy_protocol,
            connection_timeout: origin_or_global_duration(
                origin.conn_timeout.as_ref(),
                global_http.and_then(|cfg| cfg.conn_timeout.as_ref()),
            ),
            read_timeout: origin_or_global_duration(
                origin.read_timeout.as_ref(),
                global_http.and_then(|cfg| cfg.read_timeout.as_ref()),
            ),
            idle_timeout: origin_or_global_duration(
                origin.idle_timeout.as_ref(),
                global_http.and_then(|cfg| cfg.idle_timeout.as_ref()),
            ),
            write_timeout: origin_or_global_duration(
                origin.write_timeout.as_ref(),
                global_http.and_then(|cfg| cfg.write_timeout.as_ref()),
            ),
            client_cert: origin.cert.clone(),
            unsupported_reason: None,
            oss_backend: None,
        });
        backend.ext = ext;
        backend.weight = origin.weight.max(1) as usize;
        endpoints.push(backend);

        if detected_hc.is_none()
            && let Some(hc) = &origin.health_check
            && hc.is_on
        {
            detected_hc = Some((origin, hc));
        }
    }

    if endpoints.is_empty() && !unsupported_endpoints.is_empty() {
        endpoints = unsupported_endpoints;
    }

    if endpoints.is_empty() {
        return (None, false);
    }

    debug!(
        "LB Builder: Creating {:?} LB with {} endpoints",
        role,
        endpoints.len()
    );
    let mut set = BTreeSet::new();
    for endpoint in endpoints {
        set.insert(endpoint);
    }
    let backends = Backends::new(Static::new(set));
    let has_health_check = detected_hc.is_some();
    // Admin console codes (site-settings-fixtures.ts:345-356):
    //   round_robin -> "polling", weighted -> "weight",
    //   ip_hash -> "hash", least_conn -> "least_conn".
    // Empty / "random" keeps the historical Random default.
    let code = rp_cfg
        .scheduling
        .as_ref()
        .map(|scheduling| scheduling.code.as_str())
        .unwrap_or("");
    let lb = match code {
        "" | "random" => {
            AnyLoadBalancer::Random(finalize_lb(LoadBalancer::from_backends(backends), detected_hc))
        }
        "polling" | "weight" => AnyLoadBalancer::RoundRobin(finalize_lb(
            LoadBalancer::from_backends(backends),
            detected_hc,
        )),
        "hash" => {
            warn_unsupported_hash_key(rp_cfg);
            AnyLoadBalancer::Consistent(finalize_lb(
                LoadBalancer::from_backends(backends),
                detected_hc,
            ))
        }
        "least_conn" => {
            let counters = crate::origin_state::ORIGIN_STATE_MANAGER.inflight_counters();
            AnyLoadBalancer::LeastConn(finalize_lb(
                LoadBalancer::from_backends_with_config(backends, Some(counters)),
                detected_hc,
            ))
        }
        other => {
            warn!(
                "LB Builder: unknown scheduling.code {:?}; falling back to round robin (polling)",
                other
            );
            AnyLoadBalancer::RoundRobin(finalize_lb(
                LoadBalancer::from_backends(backends),
                detected_hc,
            ))
        }
    };
    (Some(Arc::new(lb)), has_health_check)
}

/// Apply the shared static-update and optional health-check setup to a freshly
/// built concrete load balancer, then return it behind an `Arc`.
fn finalize_lb<S>(
    mut lb: LoadBalancer<S>,
    detected_hc: Option<(&OriginConfig, &HealthCheckConfig)>,
) -> Arc<LoadBalancer<S>>
where
    S: BackendSelection + 'static,
    S::Iter: BackendIter,
{
    lb.update()
        .now_or_never()
        .expect("static load balancer update should not block")
        .expect("static load balancer update should not fail");

    if let Some((origin, hc_cfg)) = detected_hc {
        let frequency = hc_cfg
            .interval
            .as_ref()
            .map(crate::utils::to_duration)
            .unwrap_or(Duration::from_secs(30));
        let use_tcp = hc_cfg.protocol.as_deref() == Some("tcp");
        let hc: Box<dyn health_check::HealthCheck + Send + Sync + 'static> = if use_tcp {
            let mut hc = health_check::TcpHealthCheck::new();
            if let Some(timeout) = &hc_cfg.timeout {
                hc.peer_template.options.connection_timeout =
                    Some(crate::utils::to_duration(timeout));
            }
            hc
        } else {
            let host = origin.addr.as_ref().map(|a| a.host()).unwrap_or_default();
            let use_tls = origin.addr.as_ref().map(|a| a.is_https()).unwrap_or(false);
            let mut hc = health_check::HttpHealthCheck::new(&host, use_tls);
            if !hc_cfg.url.is_empty()
                && let Ok(uri) = hc_cfg.url.parse::<http::Uri>()
                && let Some(path_and_query) = uri.path_and_query()
            {
                let mut parts = hc.req.uri.clone().into_parts();
                parts.path_and_query = Some(path_and_query.clone());
                if let Ok(new_uri) = http::Uri::from_parts(parts) {
                    hc.req.set_uri(new_uri);
                }
            }
            if let Some(timeout) = &hc_cfg.timeout {
                hc.peer_template.options.connection_timeout =
                    Some(crate::utils::to_duration(timeout));
            }
            Box::new(hc)
        };
        lb.set_health_check(hc);
        lb.health_check_frequency = Some(frequency);
    }

    Arc::new(lb)
}

/// Warn when `scheduling.code == "hash"` carries a `hashKey` we do not
/// implement. Only `"ip"` is produced by the admin console
/// (`site-settings-fixtures.ts:422`); consistent hashing always keys on the
/// client IP here.
fn warn_unsupported_hash_key(rp_cfg: &ReverseProxyConfig) {
    let Some(scheduling) = rp_cfg.scheduling.as_ref() else {
        return;
    };
    let hash_key = scheduling
        .options
        .get("hashKey")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if !hash_key.is_empty() && !hash_key.eq_ignore_ascii_case("ip") {
        warn!(
            "LB Builder: unsupported scheduling hashKey {:?}; consistent hashing keys on the client IP",
            hash_key
        );
    }
}

fn fallback_lb() -> (Arc<AnyLoadBalancer>, bool) {
    warn!("LB Builder: No upstreams found. Falling back to 127.0.0.1:80");
    let mut b = Backend::new("127.0.0.1:80").expect("valid fallback backend");
    let mut ext = Extensions::new();
    ext.insert(BackendExtension {
        origin_role: OriginRole::Fallback,
        use_tls: false,
        host: String::new(),
        rp_host: String::new(),
        origin_id: 0,
        origin_host: String::new(),
        origin_host_normalized: String::new(),
        explicit_tls_host_normalized: None,
        follow_port: false,
        follow_host: false,
        http2_enabled: false,
        http3_enabled: false,
        tls_security_verify_mode: OriginTlsSecurityVerifyMode::Force,
        legacy_tls_verify: None,
        request_host_excluding_port: false,
        proxy_protocol: ProxyProtocolConfig::default(),
        connection_timeout: None,
        read_timeout: None,
        idle_timeout: None,
        write_timeout: None,
        client_cert: None,
        unsupported_reason: None,
        oss_backend: None,
    });
    b.ext = ext;
    let mut set = BTreeSet::new();
    set.insert(b);
    let backends = Backends::new(Static::new(set));
    let lb: LoadBalancer<RoundRobin> = LoadBalancer::from_backends(backends);
    lb.update()
        .now_or_never()
        .expect("static fallback load balancer update should not block")
        .expect("static fallback load balancer update should not fail");
    (Arc::new(AnyLoadBalancer::RoundRobin(Arc::new(lb))), false)
}

fn unsupported_origin_backend(
    origin: &OriginConfig,
    role: OriginRole,
    reason: &str,
) -> Option<Backend> {
    let mut backend = Backend::new("127.0.0.1:9").ok()?;
    let mut ext = Extensions::new();
    ext.insert(BackendExtension {
        origin_role: role,
        use_tls: false,
        host: origin.request_host.clone(),
        rp_host: String::new(),
        origin_id: origin.id,
        origin_host: String::new(),
        origin_host_normalized: String::new(),
        explicit_tls_host_normalized: None,
        follow_port: false,
        follow_host: false,
        http2_enabled: false,
        http3_enabled: false,
        tls_security_verify_mode: OriginTlsSecurityVerifyMode::Force,
        legacy_tls_verify: None,
        request_host_excluding_port: false,
        proxy_protocol: ProxyProtocolConfig::default(),
        connection_timeout: origin.conn_timeout.as_ref().map(crate::utils::to_duration),
        read_timeout: origin.read_timeout.as_ref().map(crate::utils::to_duration),
        idle_timeout: origin.idle_timeout.as_ref().map(crate::utils::to_duration),
        write_timeout: origin.write_timeout.as_ref().map(crate::utils::to_duration),
        client_cert: origin.cert.clone(),
        unsupported_reason: Some(reason.to_string()),
        oss_backend: None,
    });
    backend.ext = ext;
    backend.weight = origin.weight.max(1) as usize;
    Some(backend)
}

fn oss_origin_backend(
    origin: &OriginConfig,
    role: OriginRole,
    rp_cfg: &ReverseProxyConfig,
    allow_lan: bool,
    global_http: Option<&GlobalHTTPAllConfig>,
) -> Option<Backend> {
    let oss_backend = match crate::oss_origin::OssBackend::from_origin(origin) {
        Ok(config) => config,
        Err(err) => {
            warn!(
                "LB Builder: Skipping invalid OSS {:?} origin {}: {}",
                role, origin.id, err
            );
            return None;
        }
    };
    let mut backend =
        backend_from_oss_target(&oss_backend.connect_addr, role, origin.id, allow_lan)?;
    let legacy_tls_verify = parse_legacy_tls_verify(origin.tls_verify.as_ref());
    let mut ext = Extensions::new();
    ext.insert(BackendExtension {
        origin_role: role,
        use_tls: oss_backend.use_tls,
        host: origin.request_host.clone(),
        rp_host: String::new(),
        origin_id: origin.id,
        origin_host: oss_backend.host_header.clone(),
        origin_host_normalized: normalized_tls_host(&oss_backend.host_header),
        explicit_tls_host_normalized: explicit_tls_host(&origin.request_host, ""),
        follow_port: false,
        follow_host: false,
        http2_enabled: origin.http2_enabled,
        http3_enabled: origin.http3_enabled,
        tls_security_verify_mode: origin.tls_security_verify_mode,
        legacy_tls_verify,
        request_host_excluding_port: rp_cfg.request_host_excluding_port,
        proxy_protocol: rp_cfg.proxy_protocol,
        connection_timeout: origin_or_global_duration(
            origin.conn_timeout.as_ref(),
            global_http.and_then(|cfg| cfg.conn_timeout.as_ref()),
        ),
        read_timeout: origin_or_global_duration(
            origin.read_timeout.as_ref(),
            global_http.and_then(|cfg| cfg.read_timeout.as_ref()),
        ),
        idle_timeout: origin_or_global_duration(
            origin.idle_timeout.as_ref(),
            global_http.and_then(|cfg| cfg.idle_timeout.as_ref()),
        ),
        write_timeout: origin_or_global_duration(
            origin.write_timeout.as_ref(),
            global_http.and_then(|cfg| cfg.write_timeout.as_ref()),
        ),
        client_cert: origin.cert.clone(),
        unsupported_reason: None,
        oss_backend: Some(oss_backend),
    });
    backend.ext = ext;
    backend.weight = origin.weight.max(1) as usize;
    Some(backend)
}

fn normalized_tls_host(value: &str) -> String {
    strip_addr_port(value).to_ascii_lowercase()
}

fn explicit_tls_host(origin_host: &str, rp_host: &str) -> Option<String> {
    if !origin_host.is_empty() {
        Some(normalized_tls_host(origin_host))
    } else if !rp_host.is_empty() {
        Some(normalized_tls_host(rp_host))
    } else {
        None
    }
}

fn parse_legacy_tls_verify(value: Option<&serde_json::Value>) -> Option<bool> {
    value.map(|value| match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Object(obj) => obj
            .get("isOn")
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        serde_json::Value::Number(value) => value.as_i64().unwrap_or(1) > 0,
        serde_json::Value::String(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no" | "skip" | "none"
        ),
        _ => true,
    })
}

fn origin_or_global_duration(
    origin_value: Option<&serde_json::Value>,
    global_value: Option<&serde_json::Value>,
) -> Option<Duration> {
    origin_value
        .and_then(crate::utils::non_zero_duration)
        .or_else(|| global_value.and_then(crate::utils::non_zero_duration))
}

fn backend_from_oss_target(
    target: &str,
    role: OriginRole,
    origin_id: i64,
    allow_lan: bool,
) -> Option<Backend> {
    if !allow_lan && is_local_addr(target) {
        warn!(
            "LB Builder: Skipping OSS {:?} origin {} address {} as it is not allowed in cluster settings.",
            role, origin_id, target
        );
        return None;
    }

    if let Ok(backend) = Backend::new(target) {
        return Some(backend);
    }

    match target.to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                let resolved = addr.to_string();
                if !allow_lan && is_local_addr(&resolved) {
                    warn!(
                        "LB Builder: Skipping OSS {:?} origin {} address {} resolved to LAN address {}.",
                        role, origin_id, target, resolved
                    );
                    return None;
                }
                debug!(
                    "LB Builder: Resolved OSS {:?} origin {} address {} to {}",
                    role, origin_id, target, resolved
                );
                Backend::new(&resolved).ok()
            } else {
                warn!(
                    "LB Builder: Skipping OSS {:?} origin {} address {}: DNS returned no addresses",
                    role, origin_id, target
                );
                None
            }
        }
        Err(err) => {
            warn!(
                "LB Builder: Skipping invalid OSS {:?} origin {} address {}: {}",
                role, origin_id, target, err
            );
            None
        }
    }
}

pub async fn build_lb_blocking(
    server_id: i64,
    rp_cfg: ReverseProxyConfig,
    level: i32,
    parent_nodes: Arc<HashMap<i64, Vec<ParentNodeConfig>>>,
    tiered_origin_bypass: bool,
    allow_lan: bool,
) -> anyhow::Result<(Arc<AnyLoadBalancer>, bool)> {
    build_lb_blocking_with_global_http(
        server_id,
        rp_cfg,
        level,
        parent_nodes,
        tiered_origin_bypass,
        allow_lan,
        None,
    )
    .await
}

pub async fn build_lb_blocking_with_global_http(
    server_id: i64,
    rp_cfg: ReverseProxyConfig,
    level: i32,
    parent_nodes: Arc<HashMap<i64, Vec<ParentNodeConfig>>>,
    tiered_origin_bypass: bool,
    allow_lan: bool,
    global_http: Option<GlobalHTTPAllConfig>,
) -> anyhow::Result<(Arc<AnyLoadBalancer>, bool)> {
    tokio::task::spawn_blocking(move || {
        build_lb_with_global_http(
            server_id,
            &rp_cfg,
            level,
            parent_nodes.as_ref(),
            tiered_origin_bypass,
            allow_lan,
            global_http.as_ref(),
        )
    })
    .await
    .map_err(|err| anyhow::anyhow!("load balancer build task failed: {}", err))
}

pub fn build_parent_lb(
    cluster_id: i64,
    nodes: &[ParentNodeConfig],
    allow_lan: bool,
) -> Arc<LoadBalancer<Consistent>> {
    let mut endpoints = Vec::new();
    for node in nodes {
        let targets = node.to_addresses();
        for target in targets {
            if target.is_empty() {
                continue;
            }

            if !allow_lan && is_local_addr(&target) {
                warn!(
                    "LB Builder: Skipping LAN parent node address {} as it is not allowed.",
                    target
                );
                continue;
            }

            if let Ok(mut backend) = Backend::new(&target) {
                let mut ext = Extensions::new();
                ext.insert(BackendExtension {
                    origin_role: OriginRole::Primary,
                    use_tls: true, // L1 -> L2 always TLS by default.
                    host: String::new(),
                    rp_host: String::new(),
                    origin_id: 0,
                    origin_host: String::new(),
                    origin_host_normalized: String::new(),
                    explicit_tls_host_normalized: None,
                    follow_port: false,
                    follow_host: false,
                    http2_enabled: false,
                    http3_enabled: false,
                    tls_security_verify_mode: OriginTlsSecurityVerifyMode::Skip,
                    legacy_tls_verify: None,
                    request_host_excluding_port: false,
                    proxy_protocol: ProxyProtocolConfig::default(),
                    connection_timeout: None,
                    read_timeout: None,
                    idle_timeout: None,
                    write_timeout: None,
                    client_cert: None,
                    unsupported_reason: None,
                    oss_backend: None,
                });
                backend.ext = ext;
                backend.weight = node.weight.max(1) as usize;
                endpoints.push(backend);
            }
        }
    }

    if endpoints.is_empty()
        && let Ok(b) = Backend::new("127.0.0.1:80")
    {
        endpoints.push(b);
    }

    debug!(
        "LB Builder: Creating Parent LB for cluster {} with {} endpoints",
        cluster_id,
        endpoints.len()
    );
    let mut set = BTreeSet::new();
    for e in endpoints {
        set.insert(e);
    }
    let backends = Backends::new(Static::new(set));
    let lb = LoadBalancer::from_backends(backends);
    lb.update()
        .now_or_never()
        .expect("static parent load balancer update should not block")
        .expect("static parent load balancer update should not fail");
    Arc::new(lb)
}

fn reverse_proxy_request_host(
    rp_cfg: &ReverseProxyConfig,
    addr: &crate::config_models::FlexibleAddr,
) -> String {
    let host = match rp_cfg.request_host_type {
        // requestHostType=1 means send the origin host as upstream Host/SNI.
        1 => origin_addr_host(addr),
        // requestHostType=2 means use the reverse-proxy-level custom Host.
        2 if !rp_cfg.request_host.is_empty() => rp_cfg.request_host.clone(),
        _ => String::new(),
    };
    if rp_cfg.request_host_excluding_port {
        strip_addr_port(&host)
    } else {
        host
    }
}

fn backend_from_origin_target(
    target: &str,
    raw: &crate::config_models::FlexibleAddr,
    label: &str,
    allow_lan: bool,
) -> Option<Backend> {
    if let Ok(backend) = Backend::new(target) {
        return Some(backend);
    }

    match target.to_socket_addrs() {
        Ok(mut addrs) => {
            if let Some(addr) = addrs.next() {
                let resolved = addr.to_string();
                if !allow_lan && is_local_addr(&resolved) {
                    warn!(
                        "LB Builder: Skipping {} address {} resolved to LAN address {}.",
                        label, target, resolved
                    );
                    return None;
                }
                debug!(
                    "LB Builder: Resolved {} address {} to {}",
                    label, target, resolved
                );
                Backend::new(&resolved).ok()
            } else {
                warn!(
                    "LB Builder: Skipping {} address {} (normalized from {:?}): DNS returned no addresses",
                    label, target, raw
                );
                None
            }
        }
        Err(err) => {
            warn!(
                "LB Builder: Skipping invalid {} address {} (normalized from {:?}): {}",
                label, target, raw, err
            );
            None
        }
    }
}

fn origin_addr_host(addr: &crate::config_models::FlexibleAddr) -> String {
    let host = addr.to_address();
    if addr.is_https() {
        host.strip_suffix(":443").unwrap_or(&host).to_string()
    } else {
        host.strip_suffix(":80").unwrap_or(&host).to_string()
    }
}

pub(crate) fn strip_addr_port(value: &str) -> String {
    if let Some(rest) = value.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[..end].to_string();
    }
    if value.matches(':').count() == 1
        && let Some((host, _)) = value.rsplit_once(':')
    {
        return host.to_string();
    }
    value.to_string()
}

fn is_local_addr(addr: &str) -> bool {
    let host = addr.split(':').next().unwrap_or(addr);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => {
                let octets = v6.octets();
                (octets[0] & 0xfe == 0xfc) || v6.is_loopback()
            }
        }
    } else {
        // If it's a hostname like "localhost"
        host.eq_ignore_ascii_case("localhost")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_models::{FlexibleAddr, NetworkAddressConfig, OriginConfig};

    fn rp_cfg(request_host_type: i8, request_host: &str) -> ReverseProxyConfig {
        ReverseProxyConfig {
            is_on: true,
            primary_origins: vec![],
            backup_origins: vec![],
            scheduling: None,
            request_host: request_host.to_string(),
            request_host_type,
            request_host_excluding_port: false,
            proxy_protocol: ProxyProtocolConfig::default(),
        }
    }

    fn origin(addr: FlexibleAddr, request_host: &str) -> OriginConfig {
        OriginConfig {
            id: 1,
            name: String::new(),
            addr: Some(addr),
            is_on: true,
            weight: 1,
            health_check: None,
            request_host: request_host.to_string(),
            follow_host: false,
            follow_port: false,
            http2_enabled: false,
            http3_enabled: false,
            conn_timeout: None,
            read_timeout: None,
            idle_timeout: None,
            write_timeout: None,
            cert: None,
            tls_security_verify_mode: OriginTlsSecurityVerifyMode::Auto,
            tls_verify: None,
            oss: None,
        }
    }

    #[test]
    fn origin_timeout_zero_falls_back_to_global_http_timeout() {
        let global = Some(serde_json::json!(50));
        let other = Some(serde_json::json!(8));

        assert_eq!(
            origin_or_global_duration(Some(&serde_json::json!(0)), global.as_ref()),
            Some(Duration::from_secs(50))
        );
        assert_eq!(
            origin_or_global_duration(other.as_ref(), global.as_ref()),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            origin_or_global_duration(None, global.as_ref()),
            Some(Duration::from_secs(50))
        );
        assert_eq!(origin_or_global_duration(None, None), None);
    }

    #[test]
    fn origin_tls_security_mode_uses_real_control_plane_field() {
        let raw = serde_json::json!({
            "id": 1011,
            "addr": {"protocol": "https", "host": "a.com", "portRange": "443"},
            "tlsSecurityVerifyMode": "skip",
            "http2Enabled": true,
            "weight": 10
        });
        let origin: OriginConfig = serde_json::from_value(raw).unwrap();

        assert_eq!(
            origin.tls_security_verify_mode,
            OriginTlsSecurityVerifyMode::Skip
        );
        assert!(origin.http2_enabled);
        assert_eq!(origin.weight, 10);
    }

    #[test]
    fn origin_tls_auto_verifies_explicit_origin_host_but_not_downstream_host() {
        let mut ext = BackendExtension {
            origin_role: OriginRole::Primary,
            use_tls: true,
            host: String::new(),
            rp_host: String::new(),
            origin_id: 44,
            origin_host: "test.yhtuj.cn".to_string(),
            origin_host_normalized: "test.yhtuj.cn".to_string(),
            explicit_tls_host_normalized: None,
            follow_port: false,
            follow_host: false,
            http2_enabled: false,
            http3_enabled: false,
            tls_security_verify_mode: OriginTlsSecurityVerifyMode::Auto,
            legacy_tls_verify: None,
            request_host_excluding_port: false,
            proxy_protocol: ProxyProtocolConfig::default(),
            connection_timeout: None,
            read_timeout: None,
            idle_timeout: None,
            write_timeout: None,
            client_cert: None,
            unsupported_reason: None,
            oss_backend: None,
        };

        assert!(should_verify_origin_tls(
            &ext,
            "test.yhtuj.cn",
            Some("test2s.yhtuj.cn")
        ));
        assert!(!should_verify_origin_tls(
            &ext,
            "test2s.yhtuj.cn",
            Some("test2s.yhtuj.cn")
        ));

        ext.tls_security_verify_mode = OriginTlsSecurityVerifyMode::Force;
        assert!(should_verify_origin_tls(
            &ext,
            "test2s.yhtuj.cn",
            Some("test2s.yhtuj.cn")
        ));

        ext.tls_security_verify_mode = OriginTlsSecurityVerifyMode::Skip;
        assert!(!should_verify_origin_tls(
            &ext,
            "test.yhtuj.cn",
            Some("test2s.yhtuj.cn")
        ));
    }

    #[test]
    fn request_host_type_origin_uses_object_origin_host() {
        let addr = FlexibleAddr::Object(NetworkAddressConfig {
            protocol: Some("https".to_string()),
            host: Some("origin.example.com".to_string()),
            port_range: Some("443".to_string()),
        });

        assert_eq!(
            reverse_proxy_request_host(&rp_cfg(1, ""), &addr),
            "origin.example.com"
        );
    }

    #[test]
    fn request_host_type_origin_strips_scheme_and_port_from_string_origin() {
        let addr = FlexibleAddr::String("https://origin.example.com:443".to_string());

        assert_eq!(
            reverse_proxy_request_host(&rp_cfg(1, ""), &addr),
            "origin.example.com"
        );
    }

    #[test]
    fn request_host_type_custom_uses_configured_request_host() {
        let addr = FlexibleAddr::String("origin.example.com:443".to_string());

        assert_eq!(
            reverse_proxy_request_host(&rp_cfg(2, "custom.example.com"), &addr),
            "custom.example.com"
        );
    }

    #[test]
    fn request_host_type_proxy_server_leaves_host_unset() {
        let addr = FlexibleAddr::String("origin.example.com:443".to_string());

        assert_eq!(reverse_proxy_request_host(&rp_cfg(0, ""), &addr), "");
    }

    #[test]
    fn request_host_type_origin_keeps_non_default_port() {
        let addr = FlexibleAddr::String("http://origin.example.com:8080".to_string());

        assert_eq!(
            reverse_proxy_request_host(&rp_cfg(1, ""), &addr),
            "origin.example.com:8080"
        );
    }

    #[test]
    fn request_host_excluding_port_strips_custom_port() {
        let addr = FlexibleAddr::String("http://origin.example.com:8080".to_string());
        let mut cfg = rp_cfg(1, "");
        cfg.request_host_excluding_port = true;

        assert_eq!(
            reverse_proxy_request_host(&cfg, &addr),
            "origin.example.com"
        );
    }

    #[test]
    fn build_lb_accepts_http_url_origin_and_preserves_custom_host() {
        let mut cfg = rp_cfg(2, "custom.example.com");
        cfg.primary_origins.push(origin(
            FlexibleAddr::String("http://127.0.0.1:8080".to_string()),
            "",
        ));

        let (lb, has_hc) = build_lb(1, &cfg, 2, &HashMap::new(), false, true);
        let peer = lb.select(b"", 128).expect("origin should be selectable");
        let ext = peer
            .ext
            .get::<BackendExtension>()
            .expect("backend extension should be present");

        assert_eq!(peer.addr.to_string(), "127.0.0.1:8080");
        assert!(!ext.use_tls);
        assert_eq!(ext.rp_host, "custom.example.com");
        assert!(!has_hc);
    }

    fn origin_with_id(id: i64, addr: &str) -> OriginConfig {
        let mut origin = origin(FlexibleAddr::String(addr.to_string()), "");
        origin.id = id;
        origin
    }

    /// Build an origin pool through the production `build_lb` path for the given
    /// scheduling code and origin ids.
    fn build_any_lb(code: &str, options: serde_json::Value, ids: &[i64]) -> Arc<AnyLoadBalancer> {
        let mut cfg = rp_cfg(0, "");
        cfg.scheduling = Some(crate::config_models::SchedulingConfig {
            code: code.to_string(),
            options,
        });
        cfg.primary_origins = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                origin_with_id(*id, &format!("http://127.0.0.1:{}", 9500 + index))
            })
            .collect();
        let (lb, _hc) = build_lb(1, &cfg, 2, &HashMap::new(), false, true);
        lb
    }

    fn variant_name(lb: &AnyLoadBalancer) -> &'static str {
        match lb {
            AnyLoadBalancer::RoundRobin(_) => "RoundRobin",
            AnyLoadBalancer::Random(_) => "Random",
            AnyLoadBalancer::Consistent(_) => "Consistent",
            AnyLoadBalancer::LeastConn(_) => "LeastConn",
            AnyLoadBalancer::OriginPool { primary, .. } => variant_name(primary),
        }
    }

    #[test]
    fn scheduling_code_maps_to_the_expected_algorithm() {
        assert_eq!(variant_name(&build_any_lb("", serde_json::json!({}), &[1, 2])), "Random");
        assert_eq!(
            variant_name(&build_any_lb("random", serde_json::json!({}), &[1, 2])),
            "Random"
        );
        assert_eq!(
            variant_name(&build_any_lb("polling", serde_json::json!({}), &[1, 2])),
            "RoundRobin"
        );
        assert_eq!(
            variant_name(&build_any_lb("weight", serde_json::json!({}), &[1, 2])),
            "RoundRobin"
        );
        assert_eq!(
            variant_name(&build_any_lb(
                "hash",
                serde_json::json!({"hashKey": "ip"}),
                &[1, 2]
            )),
            "Consistent"
        );
        assert_eq!(
            variant_name(&build_any_lb("least_conn", serde_json::json!({}), &[1, 2])),
            "LeastConn"
        );
        // Unknown code falls back to round robin (explicit, deterministic).
        assert_eq!(
            variant_name(&build_any_lb("bogus", serde_json::json!({}), &[1, 2])),
            "RoundRobin"
        );
    }

    #[test]
    fn consistent_hash_is_sticky_and_advances_past_a_down_backend() {
        let lb = build_any_lb("hash", serde_json::json!({"hashKey": "ip"}), &[101, 102, 103]);
        assert_eq!(variant_name(&lb), "Consistent");
        assert!(lb.uses_client_ip_key());
        assert!(!lb.is_least_conn());
        // Not a round-robin pool.
        assert!(lb.as_round_robin().is_none());

        // Same key always returns the same backend (IP stickiness).
        let first = lb.select(b"203.0.113.7", 32).expect("backend");
        for _ in 0..32 {
            assert_eq!(
                lb.select(b"203.0.113.7", 32).expect("backend"),
                first,
                "consistent hash must be stable for a fixed key"
            );
        }

        // Distinct keys spread across more than one backend.
        let mut seen = BTreeSet::new();
        for key in 0u8..64 {
            seen.insert(lb.select(&[key], 32).expect("backend").to_string());
        }
        assert!(seen.len() > 1, "distinct keys should distribute, saw {seen:?}");

        // Health fallback: with the first pick marked down, selection must walk
        // the ring to a different backend instead of spinning on the same one.
        let first_id = peer_origin_id(&first);
        let alternate = lb
            .select_with_backup(b"203.0.113.7", 32, |origin_id| origin_id == first_id)
            .expect("fallback backend");
        assert_ne!(
            alternate, first,
            "consistent hash must advance to another backend when the first is down"
        );
    }

    #[test]
    fn least_conn_prefers_the_lowest_inflight_origin() {
        let lb = build_any_lb("least_conn", serde_json::json!({}), &[201, 202, 203]);
        assert_eq!(variant_name(&lb), "LeastConn");
        assert!(lb.is_least_conn());
        assert!(!lb.uses_client_ip_key());
        assert!(lb.as_round_robin().is_none());

        let counters = crate::origin_state::ORIGIN_STATE_MANAGER.inflight_counters();
        assert_eq!(counters.inflight(201), 0);

        // Put load on two of the three origins; the idle one must win.
        let guard_a = crate::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(201);
        let guard_b = crate::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(202);
        let guard_c = crate::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(202);
        assert_eq!(counters.inflight(202), 2);

        let peer = lb.select(b"", 16).expect("backend");
        assert_eq!(
            peer_origin_id(&peer),
            203,
            "least_conn must pick the origin with the fewest in-flight requests"
        );

        // Dropping the guards releases the slots (no leak).
        drop(guard_a);
        drop(guard_b);
        drop(guard_c);
        assert_eq!(counters.inflight(201), 0);
        assert_eq!(counters.inflight(202), 0);
    }

    #[test]
    fn least_conn_ties_break_deterministically_by_backend_order() {
        let lb = build_any_lb("least_conn", serde_json::json!({}), &[301, 302, 303]);
        // All counts equal (zero): selection is stable across repeated calls.
        let first = lb.select(b"", 16).expect("backend");
        for _ in 0..16 {
            assert_eq!(lb.select(b"", 16).expect("backend"), first);
        }
    }

    #[test]
    fn consistent_hash_with_empty_key_still_spreads() {
        // The L4 transports pass an empty key; a consistent-hash pool must not
        // pin every such request to a single backend.
        let lb = build_any_lb("hash", serde_json::json!({"hashKey": "ip"}), &[401, 402, 403]);
        let mut seen = BTreeSet::new();
        for _ in 0..128 {
            seen.insert(lb.select(b"", 16).expect("backend").to_string());
        }
        assert!(
            seen.len() > 1,
            "empty-key consistent selection must spread, saw {seen:?}"
        );
    }
}
