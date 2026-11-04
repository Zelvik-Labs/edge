//! Per-deployment, lease-bound origin paths delivered by the control plane.
//!
//! The route is compiled while applying a configuration, not fetched or inferred
//! from visitor headers. A shield is a replacement next hop, never an extra hop
//! after another shield. Existing origin health and cache invalidation remain the
//! data-plane authorities.

use crate::config_models::{OriginConfig, OriginTlsSecurityVerifyMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::net::IpAddr;

pub const AUTH_HEADER: &str = "x-zelvik-origin-token";
pub const HOP_HEADER: &str = "x-zelvik-origin-hop";
pub const DEPLOYMENT_HEADER: &str = "x-zelvik-origin-deployment";
pub(crate) const PREHEAT_HEADER: &str = "x-zelvik-origin-preheat";

struct LocalPreheatGrant {
    server_id: i64,
    hostname: String,
    target: String,
    expires: std::time::Instant,
}
static LOCAL_PREHEAT_GRANTS: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<[u8; 32], LocalPreheatGrant>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashMap::new()));

/// This capability is minted only by the authenticated control task executor.
/// It is one-shot, short-lived and bound to the exact local request target.
pub(crate) struct LocalPreheatPermit {
    nonce: [u8; 32],
    header: String,
}
impl LocalPreheatPermit {
    pub(crate) fn header(&self) -> &str {
        &self.header
    }
}
impl Drop for LocalPreheatPermit {
    fn drop(&mut self) {
        LOCAL_PREHEAT_GRANTS.lock().remove(&self.nonce);
    }
}
pub(crate) fn register_local_preheat(
    server_id: i64,
    hostname: &str,
    target: &str,
) -> Result<LocalPreheatPermit, String> {
    let now = std::time::Instant::now();
    let mut grants = LOCAL_PREHEAT_GRANTS.lock();
    grants.retain(|_, grant| grant.expires > now);
    if grants.len() >= 4096 || server_id <= 0 {
        return Err("local preheat authorization capacity or identity is invalid".into());
    }
    let nonce = rand::random::<[u8; 32]>();
    grants.insert(
        nonce,
        LocalPreheatGrant {
            server_id,
            hostname: hostname.into(),
            target: target.into(),
            expires: now + std::time::Duration::from_secs(30),
        },
    );
    Ok(LocalPreheatPermit {
        nonce,
        header: hex::encode(nonce),
    })
}
pub(crate) fn consume_local_preheat(
    server_id: i64,
    hostname: &str,
    target: &str,
    peer: IpAddr,
    method: &http::Method,
    header: &[u8],
) -> bool {
    if !peer.is_loopback() || *method != http::Method::GET {
        return false;
    }
    let mut nonce = [0u8; 32];
    if hex::decode_to_slice(header, &mut nonce).is_err() {
        return false;
    }
    let mut grants = LOCAL_PREHEAT_GRANTS.lock();
    let Some(grant) = grants.remove(&nonce) else {
        return false;
    };
    grant.expires > std::time::Instant::now()
        && grant.server_id == server_id
        && grant.hostname.eq_ignore_ascii_case(hostname)
        && grant.target == target
}

#[derive(Clone, Copy, Default)]
struct PathSampleBucket {
    second: u64,
    successes: u64,
    errors: u64,
    ttfb_total: u64,
}

/// Fixed-size, per-path request observations. Four local shards keep this off
/// the global reporting/configuration locks; request recording allocates nothing.
pub struct OriginPathObserver {
    cursor: std::sync::atomic::AtomicU64,
    shards: Box<[parking_lot::Mutex<[PathSampleBucket; 90]>; 4]>,
}
#[derive(Default)]
struct ObserverRegistry {
    entries: std::collections::HashMap<[u8; 32], std::sync::Weak<OriginPathObserver>>,
    inserts: usize,
}
static ORIGIN_PATH_OBSERVERS: std::sync::LazyLock<parking_lot::Mutex<ObserverRegistry>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(ObserverRegistry::default()));

/// Keep a window across lease/measurement-only config renewals. Weak ownership
/// means removed pools do not retain their sample storage. Changing execution
/// identity, revision or actual endpoint semantics starts a new window.
pub(crate) fn origin_path_observer(
    server_id: i64,
    route: &OriginRoute,
    path: &OriginPath,
) -> std::sync::Arc<OriginPathObserver> {
    origin_path_observer_for_kind(server_id, route, path, "passive_origin")
}

pub(crate) fn origin_path_observer_for_kind(
    server_id: i64,
    route: &OriginRoute,
    path: &OriginPath,
    source_kind: &str,
) -> std::sync::Arc<OriginPathObserver> {
    let identity = serde_json::to_vec(&serde_json::json!([
        source_kind,
        server_id,
        route.site_id,
        route.deployment_id,
        route.revision,
        route.role,
        path.id,
        path.kind,
        path.origins
    ]))
    .expect("origin execution identity is serializable");
    let key: [u8; 32] = Sha256::digest(identity).into();
    let mut registry = ORIGIN_PATH_OBSERVERS.lock();
    if let Some(observer) = registry
        .entries
        .get(&key)
        .and_then(std::sync::Weak::upgrade)
    {
        return observer;
    }
    if registry.inserts.is_multiple_of(256) {
        registry
            .entries
            .retain(|_, observer| observer.strong_count() > 0)
    }
    let observer = std::sync::Arc::new(OriginPathObserver::new());
    registry
        .entries
        .insert(key, std::sync::Arc::downgrade(&observer));
    registry.inserts = registry.inserts.wrapping_add(1);
    observer
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OriginPathWindow {
    pub window_seconds: u64,
    pub sample_count: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub ttfb_micros: Option<u64>,
    pub observed_at: Option<u64>,
    pub last_attempt_at: u64,
}
impl OriginPathObserver {
    pub(crate) fn new() -> Self {
        Self {
            cursor: std::sync::atomic::AtomicU64::new(0),
            shards: Box::new(std::array::from_fn(|_| {
                parking_lot::Mutex::new([PathSampleBucket::default(); 90])
            })),
        }
    }
    pub(crate) fn record(&self, elapsed: std::time::Duration, success: bool) {
        self.record_at(unix_now(), elapsed, success);
    }
    fn record_at(&self, now: u64, elapsed: std::time::Duration, success: bool) {
        let shard = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed) as usize
            % self.shards.len();
        let mut buckets = self.shards[shard].lock();
        let bucket = &mut buckets[now as usize % 90];
        if bucket.second != now {
            *bucket = PathSampleBucket {
                second: now,
                ..PathSampleBucket::default()
            }
        }
        if success {
            bucket.successes = bucket.successes.saturating_add(1);
            bucket.ttfb_total = bucket.ttfb_total.saturating_add(
                u64::try_from(elapsed.as_micros())
                    .unwrap_or(u64::MAX)
                    .max(1),
            );
        } else {
            bucket.errors = bucket.errors.saturating_add(1)
        }
    }
    pub(crate) fn window(&self, now: u64) -> Option<OriginPathWindow> {
        let mut out = OriginPathWindow {
            window_seconds: 90,
            sample_count: 0,
            success_count: 0,
            error_count: 0,
            ttfb_micros: None,
            observed_at: None,
            last_attempt_at: 0,
        };
        let mut total = 0u128;
        for shard in self.shards.iter() {
            for bucket in shard
                .lock()
                .iter()
                .filter(|bucket| bucket.second <= now && now - bucket.second < 90)
            {
                out.success_count = out.success_count.saturating_add(bucket.successes);
                out.error_count = out.error_count.saturating_add(bucket.errors);
                total = total.saturating_add(u128::from(bucket.ttfb_total));
                if bucket.successes > 0 {
                    out.observed_at = Some(out.observed_at.unwrap_or(0).max(bucket.second))
                }
                if bucket.successes > 0 || bucket.errors > 0 {
                    out.last_attempt_at = out.last_attempt_at.max(bucket.second)
                }
            }
        }
        out.sample_count = out.success_count.saturating_add(out.error_count);
        if out.success_count > 0 {
            out.ttfb_micros =
                Some(u64::try_from(total / u128::from(out.success_count)).unwrap_or(u64::MAX))
        }
        (out.sample_count > 0).then_some(out)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    Direct,
    FixedShield,
    SmartShield,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RouteRole {
    Ingress,
    Shield,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathKind {
    Direct,
    Shield,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginPath {
    pub id: String,
    pub kind: PathKind,
    pub priority: u32,
    pub ready: bool,
    pub valid_until: u64,
    pub origins: Vec<OriginConfig>,
    #[serde(default)]
    pub observed_at: Option<u64>,
    #[serde(default)]
    pub ttfb_micros: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginRoute {
    pub site_id: String,
    pub deployment_id: String,
    pub revision: u64,
    pub valid_until: u64,
    pub mode: RouteMode,
    pub role: RouteRole,
    pub paths: Vec<OriginPath>,
    pub cache_namespace: String,
    #[serde(default)]
    pub allow_direct_fallback: bool,
    #[serde(default)]
    pub selected_path_id: Option<String>,
    #[serde(default)]
    pub hold_until: u64,
    #[serde(default = "default_improvement")]
    pub min_improvement_percent: u8,
    #[serde(default = "default_measurement_age")]
    pub measurement_max_age_seconds: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
}

fn default_improvement() -> u8 {
    10
}
fn default_measurement_age() -> u64 {
    90
}
fn default_max_attempts() -> u8 {
    2
}

/// An outgoing authentication value is only present in the encrypted node
/// configuration. Debug representations never reveal it.
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginAuthentication {
    pub deployment_id: String,
    pub token: String,
}
impl std::fmt::Debug for OriginAuthentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginAuthentication")
            .field("deployment_id", &self.deployment_id)
            .field("token", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginIngress {
    pub site_id: String,
    pub deployment_id: String,
    pub hostname: String,
    pub token_sha256: String,
    pub allowed_cidrs: Vec<ipnet::IpNet>,
    pub valid_until: u64,
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0)
}

impl OriginIngress {
    pub fn validate(&self, now: u64) -> Result<(), String> {
        if self.site_id.is_empty() || self.deployment_id.is_empty() || self.hostname.is_empty() {
            return Err("origin ingress requires site, deployment and hostname".into());
        }
        if self.allowed_cidrs.is_empty() || self.valid_until <= now {
            return Err("origin ingress requires an unexpired source authorization".into());
        }
        let mut digest = [0u8; 32];
        hex::decode_to_slice(&self.token_sha256, &mut digest)
            .map_err(|_| "origin ingress token digest must be SHA256 hex")?;
        Ok(())
    }

    pub fn authorizes(
        &self,
        now: u64,
        peer: IpAddr,
        hostname: &str,
        token: &[u8],
        deployment: &str,
        hop: Option<&str>,
    ) -> bool {
        if self.valid_until <= now
            || token.is_empty()
            || !self.hostname.eq_ignore_ascii_case(hostname)
            || deployment != self.deployment_id
            || !matches!(hop, None | Some("1"))
            || !self.allowed_cidrs.iter().any(|net| net.contains(&peer))
        {
            return false;
        }
        let mut expected = [0u8; 32];
        if hex::decode_to_slice(&self.token_sha256, &mut expected).is_err() {
            return false;
        }
        let actual = Sha256::digest(token);
        expected
            .iter()
            .zip(actual.iter())
            .fold(0u8, |difference, (a, b)| {
                difference | std::hint::black_box(a ^ b)
            })
            == 0
    }
}

impl OriginRoute {
    pub fn validate(&self, now: u64, public_names: &[String]) -> Result<(), String> {
        if self.site_id.is_empty()
            || self.deployment_id.is_empty()
            || self.revision == 0
            || self.cache_namespace.is_empty()
        {
            return Err("origin route requires identity, revision and content namespace".into());
        }
        if self.valid_until <= now || self.paths.is_empty() {
            return Err("origin route has expired or has no paths".into());
        }
        if self.role == RouteRole::Shield && self.mode != RouteMode::Direct {
            return Err("a shield must use direct customer-origin mode".into());
        }
        if self.max_attempts == 0
            || self.max_attempts > 3
            || self.min_improvement_percent > 100
            || self.measurement_max_age_seconds == 0
        {
            return Err("origin route retry or measurement budget is invalid".into());
        }
        let mut paths = HashSet::new();
        let mut origins = HashSet::new();
        for path in &self.paths {
            if path.id.is_empty() || !paths.insert(&path.id) || path.origins.is_empty() {
                return Err("origin paths must have unique identities and endpoints".into());
            }
            if self.role == RouteRole::Shield && path.kind != PathKind::Direct {
                return Err("a shield must not forward to another shield".into());
            }
            if self.mode == RouteMode::Direct && path.kind != PathKind::Direct {
                return Err("direct origin mode cannot contain a shield".into());
            }
            if self.mode != RouteMode::Direct
                && path.kind == PathKind::Direct
                && !self.allow_direct_fallback
            {
                return Err("direct fallback has not been authorized".into());
            }
            if path.observed_at.is_some_and(|time| time > now) {
                return Err("origin path observation is in the future".into());
            }
            for origin in &path.origins {
                if origin.id <= 0
                    || !origins.insert(origin.id)
                    || !origin.is_on
                    || origin.weight == 0
                {
                    return Err(
                        "managed origins require unique identities and positive weights".into(),
                    );
                }
                let Some(address) = origin.addr.as_ref() else {
                    return Err("managed origin is missing its address".into());
                };
                let host = address.host();
                if public_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&host))
                {
                    return Err("origin path points back to a public site hostname".into());
                }
                if address.is_oss() {
                    return Err("managed origin routes require explicit HTTP(S) endpoints".into());
                }
                if address.is_https()
                    && (origin.tls_security_verify_mode == OriginTlsSecurityVerifyMode::Skip
                        || crate::lb_factory::parse_legacy_tls_verify(origin.tls_verify.as_ref())
                            == Some(false))
                {
                    return Err("managed origin TLS verification cannot be disabled".into());
                }
                if path.kind == PathKind::Shield
                    && (!address.is_https() || origin.origin_authentication.is_none())
                {
                    return Err(
                        "shield endpoints require HTTPS and deployment authentication".into(),
                    );
                }
                if path.kind == PathKind::Direct && origin.origin_authentication.is_some() {
                    return Err("shield authentication cannot be sent to a customer origin".into());
                }
                if let Some(auth) = &origin.origin_authentication
                    && (auth.deployment_id != self.deployment_id
                        || auth.token.len() < 32
                        || auth.token.bytes().any(|byte| byte < 0x21 || byte > 0x7e))
                {
                    return Err("origin authentication identity or value is invalid".into());
                }
            }
        }
        if self.mode != RouteMode::Direct
            && !self.paths.iter().any(|path| path.kind == PathKind::Shield)
        {
            return Err("shield mode requires an authorized shield path".into());
        }
        if self
            .selected_path_id
            .as_ref()
            .is_some_and(|id| !paths.contains(id))
        {
            return Err("selected origin path does not exist".into());
        }
        Ok(())
    }

    fn eligible(&self, path: &OriginPath, now: u64) -> bool {
        path.ready
            && path.valid_until > now
            && (self.mode == RouteMode::Direct
                || path.kind == PathKind::Shield
                || self.allow_direct_fallback)
    }

    fn fresh_cost(&self, path: &OriginPath, now: u64) -> Option<u64> {
        let observed = path.observed_at?;
        if observed > now || now - observed > self.measurement_max_age_seconds {
            return None;
        }
        path.ttfb_micros.filter(|cost| *cost > 0)
    }

    pub fn selected_path(&self, now: u64) -> Option<&OriginPath> {
        if now >= self.valid_until {
            return None;
        }
        let fallback = self
            .paths
            .iter()
            .filter(|path| self.eligible(path, now))
            .min_by_key(|path| {
                (
                    path.kind == PathKind::Direct && self.mode != RouteMode::Direct,
                    path.priority,
                    &path.id,
                )
            })?;
        if self.mode != RouteMode::SmartShield {
            return Some(fallback);
        }
        let current = self
            .selected_path_id
            .as_ref()
            .and_then(|id| {
                self.paths
                    .iter()
                    .find(|path| &path.id == id && self.eligible(path, now))
            })
            .unwrap_or(fallback);
        if now < self.hold_until {
            return Some(current);
        }
        let Some(best) = self
            .paths
            .iter()
            .filter(|path| self.eligible(path, now))
            .filter_map(|path| self.fresh_cost(path, now).map(|cost| (path, cost)))
            .min_by_key(|(path, cost)| (*cost, path.priority, &path.id))
        else {
            return Some(fallback);
        };
        let Some(current_cost) = self.fresh_cost(current, now) else {
            return Some(fallback);
        };
        let benefit = current_cost.saturating_sub(best.1) as u128 * 100;
        if benefit >= current_cost as u128 * self.min_improvement_percent as u128 {
            Some(best.0)
        } else {
            Some(current)
        }
    }
}

pub fn bind_cache_namespace(route: Option<&OriginRoute>, key: String) -> String {
    let Some(route) = route else {
        return key;
    };
    // Length-prefixed identity prevents custom cache keys escaping a site or
    // explicit content-equivalence namespace. Different deployments only share
    // when the controller deliberately supplies the same namespace and site.
    format!(
        "{}:{}{}:{}{}",
        route.site_id.len(),
        route.site_id,
        route.cache_namespace.len(),
        route.cache_namespace,
        key
    )
}

pub fn retry_allowed(method: &http::Method, failures: u8, max_attempts: u8) -> bool {
    matches!(*method, http::Method::GET | http::Method::HEAD) && failures < max_attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_origin_window_expires_samples_and_keeps_ttfb_success_only() {
        let observer = OriginPathObserver::new();
        assert!(observer.window(100).is_none());
        observer.record_at(100, std::time::Duration::from_micros(10000), true);
        observer.record_at(101, std::time::Duration::from_micros(20000), true);
        observer.record_at(102, std::time::Duration::from_micros(90000), false);
        let window = observer.window(102).unwrap();
        assert_eq!(window.sample_count, 3);
        assert_eq!(window.success_count, 2);
        assert_eq!(window.error_count, 1);
        assert_eq!(window.ttfb_micros, Some(15000));
        assert_eq!(window.observed_at, Some(101));
        assert_eq!(window.last_attempt_at, 102);
        let window = observer.window(190).unwrap();
        assert_eq!(window.sample_count, 2);
        assert_eq!(window.ttfb_micros, Some(20000));
        assert!(observer.window(192).is_none());
    }

    #[test]
    fn local_preheat_capability_is_one_shot_and_bound_to_transport_and_target() {
        let permit = register_local_preheat(91, "content.example", "/asset?v=1").unwrap();
        assert!(!consume_local_preheat(
            91,
            "content.example",
            "/asset?v=1",
            "192.0.2.1".parse().unwrap(),
            &http::Method::GET,
            permit.header().as_bytes()
        ));
        assert!(consume_local_preheat(
            91,
            "content.example",
            "/asset?v=1",
            "127.0.0.1".parse().unwrap(),
            &http::Method::GET,
            permit.header().as_bytes()
        ));
        assert!(!consume_local_preheat(
            91,
            "content.example",
            "/asset?v=1",
            "127.0.0.1".parse().unwrap(),
            &http::Method::GET,
            permit.header().as_bytes()
        ));
        let permit = register_local_preheat(91, "content.example", "/asset?v=1").unwrap();
        assert!(!consume_local_preheat(
            92,
            "content.example",
            "/asset?v=1",
            "127.0.0.1".parse().unwrap(),
            &http::Method::GET,
            permit.header().as_bytes()
        ));
        let permit = register_local_preheat(91, "content.example", "/asset?v=1").unwrap();
        let value = permit.header().to_string();
        drop(permit);
        assert!(!consume_local_preheat(
            91,
            "content.example",
            "/asset?v=1",
            "127.0.0.1".parse().unwrap(),
            &http::Method::GET,
            value.as_bytes()
        ));
    }
    fn route() -> OriginRoute {
        serde_json::from_value(serde_json::json!({
            "siteId":"site-a", "deploymentId":"dep-a", "revision":1, "validUntil":1000,
            "mode":"smart_shield", "role":"ingress", "cacheNamespace":"content-v1",
            "selectedPathId":"hk", "holdUntil":0,
            "paths":[
                {"id":"hk","kind":"shield","priority":1,"ready":true,"validUntil":900,
                 "observedAt":100,"ttfbMicros":10000,"origins":[{"id":1,"weight":1,"addr":"https://shield-hk.example:443",
                    "originAuthentication":{"deploymentId":"dep-a","token":"0123456789abcdef0123456789abcdef"}}]},
                {"id":"sg","kind":"shield","priority":2,"ready":true,"validUntil":900,
                 "observedAt":100,"ttfbMicros":7000,"origins":[{"id":2,"weight":1,"addr":"https://shield-sg.example:443",
                    "originAuthentication":{"deploymentId":"dep-a","token":"0123456789abcdef0123456789abcdef"}}]}
            ]
        })).unwrap()
    }
    #[test]
    fn smart_route_requires_fresh_comparable_samples_and_hysteresis() {
        let mut plan = route();
        assert!(plan.validate(110, &[]).is_ok());
        assert_eq!(plan.selected_path(110).unwrap().id, "sg");
        plan.paths[1].ttfb_micros = Some(9500);
        assert_eq!(plan.selected_path(110).unwrap().id, "hk");
        plan.paths[1].ttfb_micros = Some(7000);
        plan.hold_until = 150;
        assert_eq!(plan.selected_path(110).unwrap().id, "hk");
        assert_eq!(plan.selected_path(300).unwrap().id, "hk");
        assert!(plan.selected_path(1000).is_none());
    }
    #[test]
    fn shield_chains_direct_fallback_and_tls_bypass_are_rejected() {
        let mut plan = route();
        plan.role = RouteRole::Shield;
        assert!(plan.validate(110, &[]).is_err());
        plan.role = RouteRole::Ingress;
        plan.paths[1].kind = PathKind::Direct;
        plan.paths[1].origins[0].origin_authentication = None;
        assert!(plan.validate(110, &[]).is_err());
        plan.allow_direct_fallback = true;
        assert!(plan.validate(110, &[]).is_ok());
        plan.paths[0].origins[0].tls_security_verify_mode = OriginTlsSecurityVerifyMode::Skip;
        assert!(plan.validate(110, &[]).is_err());
    }
    #[test]
    fn public_hostname_loop_and_wrong_deployment_auth_are_rejected() {
        let mut plan = route();
        assert!(plan.validate(110, &["shield-hk.example".into()]).is_err());
        plan.paths[0].origins[0]
            .origin_authentication
            .as_mut()
            .unwrap()
            .deployment_id = "dep-b".into();
        assert!(plan.validate(110, &[]).is_err());
    }
    #[test]
    fn ingress_requires_real_peer_deployment_and_token() {
        let token = b"a secret token that is longer than thirty two characters";
        let ingress = OriginIngress {
            site_id: "site-a".into(),
            deployment_id: "dep-a".into(),
            hostname: "origin.example".into(),
            token_sha256: hex::encode(Sha256::digest(token)),
            allowed_cidrs: vec!["192.0.2.0/24".parse().unwrap()],
            valid_until: 200,
        };
        assert!(ingress.validate(100).is_ok());
        let peer = "192.0.2.10".parse().unwrap();
        assert!(ingress.authorizes(100, peer, "origin.example", token, "dep-a", Some("1")));
        assert!(!ingress.authorizes(100, peer, "origin.example", b"forged", "dep-a", None));
        assert!(!ingress.authorizes(
            100,
            "198.51.100.1".parse().unwrap(),
            "origin.example",
            token,
            "dep-a",
            None
        ));
        assert!(!ingress.authorizes(100, peer, "origin.example", token, "dep-b", None));
        assert!(!ingress.authorizes(100, peer, "origin.example", token, "dep-a", Some("2")));
        assert!(!ingress.authorizes(200, peer, "origin.example", token, "dep-a", None));
    }
    #[test]
    fn content_namespace_and_retry_boundaries() {
        let mut plan = route();
        let first = bind_cache_namespace(Some(&plan), "https://example/a".into());
        plan.deployment_id = "another-cloud".into();
        assert_eq!(
            first,
            bind_cache_namespace(Some(&plan), "https://example/a".into())
        );
        plan.cache_namespace = "different-content".into();
        assert_ne!(
            first,
            bind_cache_namespace(Some(&plan), "https://example/a".into())
        );
        assert!(retry_allowed(&http::Method::GET, 1, 2));
        assert!(!retry_allowed(&http::Method::GET, 2, 2));
        assert!(!retry_allowed(&http::Method::POST, 0, 2));
        assert!(!retry_allowed(&http::Method::PUT, 0, 2));
    }
}

#[cfg(test)]
mod runtime_tests;
