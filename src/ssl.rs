use arc_swap::ArcSwap;
use base64::{Engine as _, engine::general_purpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{Arc, RwLock};
use x509_parser::prelude::*;

use crate::config_models::{SSLCertConfig, SSLPolicyConfig};
use crate::tls_crypto::{default_crypto_provider, install_default_crypto_provider};

type CertCacheMap = Arc<RwLock<HashMap<i64, (u64, Arc<CertPair>)>>>;

/// The selector that most recently published a certificate snapshot.
///
/// `DynamicCertSelector` is cheap to clone (all state is behind `Arc`s), so the
/// sync path records a handle here for read-only observability. Node status
/// reads the *published snapshot* through this handle; it never reconstructs
/// loaded state from the desired configuration.
static SSL_SELECTOR: RwLock<Option<DynamicCertSelector>> = RwLock::new(None);

#[derive(Clone)]
pub struct DynamicCertSelector {
    snapshot: Arc<ArcSwap<CertSnapshot>>,
    cache: CertCacheMap,
    ocsp_records: Arc<RwLock<HashMap<i64, OcspRecord>>>,
}

impl std::fmt::Debug for DynamicCertSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicCertSelector")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default)]
struct CertSnapshot {
    exact: HashMap<String, Arc<CertPair>>,
    wildcard: HashMap<String, Arc<CertPair>>,
    default: Option<Arc<CertPair>>,
    /// Last successfully applied server binding per runtime name. Only active
    /// server TLS policies populate this, and it is never used as a general
    /// certificate fallback: a binding is retained solely to survive an invalid
    /// replacement of the *same authorized certificate identity*.
    last_good: HashMap<String, Arc<CertPair>>,
    /// Per-hostname TLS material actually published by a server binding. This
    /// is the loaded state only; it is never populated from a desired config.
    loaded_bindings: HashMap<String, LoadedBinding>,
}

/// Loaded TLS material for one server runtime name.
#[derive(Clone)]
struct LoadedBinding {
    server_id: i64,
    pair: Arc<CertPair>,
    /// True when an invalid replacement caused the previous certificate to be
    /// kept, so consumers can distinguish desired/loaded drift explicitly.
    retained_previous: bool,
}

/// Stable identity of configured certificate material.
///
/// Persisted certificates use their positive platform ID, which survives a
/// material replacement (a new key/cert revision under the same certificate
/// record). Unsaved inline certificates have no stable ID, so their exact
/// material is the only honest identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CertIdentity {
    Id(i64),
    Material(u64),
}

#[derive(Clone, Debug)]
pub struct CertPair {
    pub id: i64,
    certified_key: Arc<CertifiedKey>,
    cert_bytes: Vec<u8>,
    key_bytes: Vec<u8>,
    ocsp: Arc<ArcSwap<OcspState>>,
    ocsp_enabled: bool,
}

#[derive(Clone, Debug)]
struct OcspRecord {
    data: Vec<u8>,
    expires_at: i64,
}

impl OcspRecord {
    fn valid(&self) -> bool {
        self.expires_at > chrono::Utc::now().timestamp() && !self.data.is_empty()
    }
}

#[derive(Debug, Default)]
struct OcspState {
    record: Option<OcspRecord>,
    key: Option<Arc<CertifiedKey>>,
}

impl Default for DynamicCertSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl DynamicCertSelector {
    pub fn new() -> Self {
        install_default_crypto_provider();
        Self {
            snapshot: Arc::new(ArcSwap::from_pointee(CertSnapshot::default())),
            cache: Arc::new(RwLock::new(HashMap::new())),
            ocsp_records: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn update_ocsp_with_expiry(
        &self,
        cert_id: i64,
        data: Vec<u8>,
        expires_at: i64,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(data.len() <= 1024 * 1024, "OCSP response exceeds 1 MiB");
        let mut records = self.ocsp_records.write().unwrap_or_else(|e| e.into_inner());
        records.retain(|_, record| record.valid());
        let record = OcspRecord { data, expires_at };
        if record.valid() {
            anyhow::ensure!(
                records.contains_key(&cert_id) || records.len() < 10_000,
                "OCSP retention capacity exceeded"
            );
            let retained_bytes: usize = records
                .iter()
                .filter(|(id, _)| **id != cert_id)
                .map(|(_, record)| record.data.len())
                .sum();
            anyhow::ensure!(
                retained_bytes + record.data.len() <= 64 * 1024 * 1024,
                "OCSP retention exceeds 64 MiB"
            );
        }
        let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, pair)) = cache.get(&cert_id) {
            pair.apply_ocsp(&record);
        }
        if record.valid() {
            records.insert(cert_id, record);
        } else {
            records.remove(&cert_id);
        }
        Ok(())
    }

    pub async fn export_default_pem(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let snapshot = self.snapshot.load();
        let pair = snapshot.default.as_ref()?.clone();
        Some((pair.cert_bytes.clone(), pair.key_bytes.clone()))
    }

    pub fn export_pair_pem_for_host(&self, host: &str) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        self.find_pair_blocking(&host.to_ascii_lowercase())
            .map(|pair| serialize_pair_pem(&pair))
    }

    pub fn export_default_pair_pem(&self) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let snapshot = self.snapshot.load();
        Some(serialize_pair_pem(snapshot.default.as_ref()?))
    }

    pub async fn export_snapshot_pem(
        &self,
    ) -> Option<(
        std::collections::HashMap<String, (Vec<u8>, Vec<u8>, Vec<u8>)>,
        std::collections::HashMap<String, (Vec<u8>, Vec<u8>, Vec<u8>)>,
        (Vec<u8>, Vec<u8>, Vec<u8>),
    )> {
        let snapshot = self.snapshot.load();

        let mut exact_out = std::collections::HashMap::new();
        for (name, pair) in snapshot.exact.iter() {
            exact_out.insert(name.clone(), serialize_pair_pem(pair));
        }

        let mut wildcard_out = std::collections::HashMap::new();
        for (name, pair) in snapshot.wildcard.iter() {
            wildcard_out.insert(name.clone(), serialize_pair_pem(pair));
        }

        let default_pair = serialize_pair_pem(snapshot.default.as_ref()?);
        Some((exact_out, wildcard_out, default_pair))
    }

    pub fn resolve_certified_key_for_host(&self, host: &str) -> Option<Arc<CertifiedKey>> {
        self.find_pair_blocking(&host.to_ascii_lowercase())
            .map(|pair| pair.resolved_certified_key())
    }

    /// Read-only observation of the certificate material this selector has
    /// actually published for server hostname bindings.
    ///
    /// Everything is derived from the current snapshot (the pairs handed to the
    /// TLS resolver), never from a desired configuration: an invalid replacement
    /// that fell back to last-known-good is reported with
    /// `retainedPrevious=true` and the SHA-256 of the *loaded* leaf. Only the
    /// leaf certificate is hashed; private keys are never exposed.
    ///
    /// The method takes one `ArcSwap` snapshot load and allocates only when the
    /// caller asks for the report, so it adds no request-path locking.
    pub fn loaded_certificate_bindings(&self) -> Value {
        let snapshot = self.snapshot.load();
        let mut loaded: Vec<(&str, &LoadedBinding)> = snapshot
            .loaded_bindings
            .iter()
            .map(|(hostname, binding)| (hostname.as_str(), binding))
            .collect();
        loaded.sort_by(|(a_host, a), (b_host, b)| {
            a.server_id
                .cmp(&b.server_id)
                .then_with(|| a_host.cmp(b_host))
        });
        Value::Array(
            loaded
                .into_iter()
                .map(|(hostname, binding)| {
                    let (sha256, not_after) = loaded_leaf_identity(&binding.pair);
                    serde_json::json!({
                        "serverId": binding.server_id,
                        "hostname": hostname,
                        "certificateId": binding.pair.id,
                        "sha256": sha256,
                        "notAfter": not_after,
                        "retainedPrevious": binding.retained_previous,
                    })
                })
                .collect(),
        )
    }

    pub fn has_default_cert(&self) -> bool {
        self.snapshot.load().default.is_some()
    }

    fn find_pair_blocking(&self, host: &str) -> Option<Arc<CertPair>> {
        let snapshot = self.snapshot.load();
        if !host.is_empty() {
            if let Some(pair) = snapshot.exact.get(host) {
                return Some(pair.clone());
            }

            if let Some(pos) = host.find('.') {
                let suffix = &host[pos..];
                let wildcard_key = format!("*{}", suffix);
                if let Some(pair) = snapshot.wildcard.get(&wildcard_key) {
                    return Some(pair.clone());
                }
            }
        }

        snapshot.default.clone()
    }

    #[doc(hidden)]
    pub fn bench_find_pair(&self, host: &str) -> bool {
        self.find_pair_blocking(host).is_some()
    }
}

/// Read-only loaded binding report for node status.
///
/// Delegates to the published snapshot of the selector that last completed a
/// certificate sync. Returns an empty array before the first sync; it never
/// falls back to desired configuration.
pub fn loaded_certificate_bindings() -> Value {
    let selector = SSL_SELECTOR
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    selector
        .map(|selector| selector.loaded_certificate_bindings())
        .unwrap_or_else(|| Value::Array(Vec::new()))
}

impl ResolvesServerCert for DynamicCertSelector {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.resolve_certified_key_for_host(client_hello.server_name().unwrap_or(""))
    }
}

/// Minimum TLS protocol version an HTTPS listener will accept.
///
/// rustls only implements TLS 1.2 and TLS 1.3, so the admin-console
/// `sslPolicy.minVersion` string is folded onto this two-valued floor: any
/// value below TLS 1.2 (or an unrecognized one) is rejected with a warning and
/// resolved to [`TlsMinVersion::Tls12`] rather than silently acting as the
/// default.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum TlsMinVersion {
    /// TLS 1.2 and TLS 1.3 — the permissive floor, also the default when unset.
    #[default]
    Tls12,
    /// TLS 1.3 only.
    Tls13,
}

impl TlsMinVersion {
    /// Resolves the `sslPolicy.minVersion` string into a supported floor.
    ///
    /// An empty string (unset) means "TLS 1.2 and above". `TLSv1.0`/`TLSv1.1`
    /// are unsupported by rustls and are explicitly rejected with a warning,
    /// degrading to the TLS 1.2 floor; any other unrecognized value is likewise
    /// warned about and resolved to the TLS 1.2 floor — never silently treated
    /// as a default.
    pub fn from_config_str(min_version: &str) -> Self {
        let trimmed = min_version.trim();
        if trimmed.is_empty() {
            return Self::Tls12;
        }
        if trimmed.eq_ignore_ascii_case("TLSv1.3") || trimmed.eq_ignore_ascii_case("TLS1.3") {
            return Self::Tls13;
        }
        if trimmed.eq_ignore_ascii_case("TLSv1.2") || trimmed.eq_ignore_ascii_case("TLS1.2") {
            return Self::Tls12;
        }
        if trimmed.eq_ignore_ascii_case("TLSv1.1")
            || trimmed.eq_ignore_ascii_case("TLS1.1")
            || trimmed.eq_ignore_ascii_case("TLSv1.0")
            || trimmed.eq_ignore_ascii_case("TLS1.0")
            || trimmed.eq_ignore_ascii_case("TLSv1")
            || trimmed.eq_ignore_ascii_case("TLS1")
        {
            tracing::warn!(
                "sslPolicy.minVersion={:?} is not supported by rustls (TLS 1.0/1.1 are disabled); \
                 falling back to the TLS 1.2 minimum",
                min_version
            );
            return Self::Tls12;
        }
        tracing::warn!(
            "sslPolicy.minVersion={:?} is not a recognized TLS version; \
             falling back to the TLS 1.2 minimum",
            min_version
        );
        Self::Tls12
    }

    /// The more permissive of two floors — used when several servers share one
    /// acceptor, which cannot vary its version set per SNI. TLS 1.2 wins unless
    /// both sides require TLS 1.3.
    fn most_permissive(self, other: Self) -> Self {
        match (self, other) {
            (Self::Tls13, Self::Tls13) => Self::Tls13,
            _ => Self::Tls12,
        }
    }
}

/// Resolved TLS policy for an HTTP/1.1+2 listener — the ALPN and protocol-version
/// knobs a single `rustls::ServerConfig` can express.
///
/// A shared acceptor cannot vary these per SNI, so when several HTTPS servers
/// bind the same address their policies are aggregated with [`TlsPolicy::union`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TlsPolicy {
    /// Whether the listener advertises `h2` in ALPN (HTTP/2 support).
    pub http2_enabled: bool,
    /// Lowest TLS version the listener accepts.
    pub min_version: TlsMinVersion,
}

impl Default for TlsPolicy {
    /// HTTP/2 与 TLS 1.2 下限是边缘的既有行为：站点没有下发 `sslPolicy` 时
    /// 不能因为「字段缺失」就静默关掉 h2，只有显式 `http2Enabled=false` 才关。
    fn default() -> Self {
        Self {
            http2_enabled: true,
            min_version: TlsMinVersion::Tls12,
        }
    }
}

impl TlsPolicy {
    /// Derives the listener policy from a server's `sslPolicy` block.
    pub fn from_ssl_policy(policy: &SSLPolicyConfig) -> Self {
        Self {
            http2_enabled: policy.http2_enabled,
            min_version: TlsMinVersion::from_config_str(&policy.min_version),
        }
    }

    /// Union of two policies sharing one acceptor: HTTP/2 is advertised if
    /// either side enables it, and the version floor is the more permissive of
    /// the two.
    pub fn union(self, other: Self) -> Self {
        Self {
            http2_enabled: self.http2_enabled || other.http2_enabled,
            min_version: self.min_version.most_permissive(other.min_version),
        }
    }
}

pub fn build_rustls_server_config(
    cert_selector: Arc<DynamicCertSelector>,
    alpn_protocols: Vec<Vec<u8>>,
    tls13_only: bool,
) -> anyhow::Result<rustls::ServerConfig> {
    anyhow::ensure!(
        cert_selector.has_default_cert(),
        "no certificate snapshot available for TLS listener"
    );

    let provider = default_crypto_provider();
    let builder = rustls::ServerConfig::builder_with_provider(provider.into());
    let builder = if tls13_only {
        builder.with_protocol_versions(&[&rustls::version::TLS13])?
    } else {
        builder.with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
    };
    let resolver: Arc<dyn ResolvesServerCert> = cert_selector;
    let mut config = builder.with_no_client_auth().with_cert_resolver(resolver);
    config.alpn_protocols = alpn_protocols;
    Ok(config)
}

/// Builds the rustls server config for an HTTP/1.1+2 listener from a resolved
/// [`TlsPolicy`]: ALPN advertises `h2` only when HTTP/2 is enabled, and the
/// accepted protocol versions follow the configured minimum.
pub fn build_rustls_server_config_for_policy(
    cert_selector: Arc<DynamicCertSelector>,
    policy: TlsPolicy,
) -> anyhow::Result<rustls::ServerConfig> {
    let alpn_protocols = if policy.http2_enabled {
        vec![b"h2".to_vec(), b"http/1.1".to_vec()]
    } else {
        vec![b"http/1.1".to_vec()]
    };
    let tls13_only = policy.min_version == TlsMinVersion::Tls13;
    build_rustls_server_config(cert_selector, alpn_protocols, tls13_only)
}

impl CertPair {
    fn resolved_certified_key(&self) -> Arc<CertifiedKey> {
        let state = self.ocsp.load();
        if self.ocsp_enabled
            && state.record.as_ref().is_some_and(OcspRecord::valid)
            && let Some(key) = &state.key
        {
            return key.clone();
        }
        self.certified_key.clone()
    }

    fn apply_ocsp(&self, record: &OcspRecord) {
        self.ocsp.store(Arc::new(OcspState {
            record: Some(record.clone()),
            key: record.valid().then(|| {
                Arc::new(build_resolved_certified_key(
                    &self.certified_key,
                    &record.data,
                ))
            }),
        }));
    }
}

fn build_resolved_certified_key(base: &CertifiedKey, ocsp: &[u8]) -> CertifiedKey {
    if ocsp.is_empty() {
        return (*base).clone();
    }
    let mut certified_key = (*base).clone();
    certified_key.ocsp = Some(ocsp.to_vec());
    certified_key
}

fn serialize_pair_pem(pair: &Arc<CertPair>) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        pair.cert_bytes.clone(),
        pair.key_bytes.clone(),
        if pair.ocsp_enabled {
            pair.ocsp
                .load()
                .record
                .as_ref()
                .filter(|record| record.valid())
                .map(|record| record.data.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        },
    )
}

pub async fn sync_certs(cert_selector: &DynamicCertSelector, certs: &[SSLCertConfig]) {
    sync_certs_for_servers(cert_selector, certs, &[]).await;
}

pub async fn sync_certs_for_servers(
    cert_selector: &DynamicCertSelector,
    certs: &[SSLCertConfig],
    servers: &[Arc<crate::config_models::ServerConfig>],
) {
    // Same lock order as OCSP updates: retain records through snapshot publication.
    let records = cert_selector
        .ocsp_records
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let old_snapshot = cert_selector.snapshot.load_full();
    let now = chrono::Utc::now().timestamp();
    let mut new_exact = HashMap::new();
    let mut new_wildcard = HashMap::new();
    let mut default_pair: Option<Arc<CertPair>> = None;
    let mut fallback_pair: Option<Arc<CertPair>> = None;
    let mut new_cache = HashMap::new();
    let mut global_authorized = HashSet::new();
    let mut retained_last_good = 0usize;
    let mut dropped_last_good = 0usize;

    let (stats_parsed, stats_reused, stats_parsed_fresh) = {
        let old_cache = cert_selector
            .cache
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut parsed = 0;
        let mut reused = 0;
        let mut parsed_fresh = 0;

        for cert_cfg in certs {
            if !cert_cfg.is_on {
                continue;
            }
            if let Some(identity) = cert_identity_from_config(cert_cfg) {
                global_authorized.insert(identity);
            }

            let cert_id = cert_cfg.id;
            let (Some(c), Some(k)) = (&cert_cfg.cert_data_json, &cert_cfg.key_data_json) else {
                continue;
            };
            let (Some(cert_bytes), Some(key_bytes)) =
                (cert_value_to_bytes(c), cert_value_to_bytes(k))
            else {
                tracing::error!(
                    "SSL Parse Error for ID {}: unsupported certificate/key JSON shape (cert_type={}, key_type={})",
                    cert_id,
                    json_value_kind(c),
                    json_value_kind(k)
                );
                continue;
            };

            let current_fingerprint =
                fnv_hash64_bytes(&[cert_bytes.as_slice(), key_bytes.as_slice()]);

            let pair = if let Some((old_fp, old_pair)) = old_cache.get(&cert_id)
                && *old_fp == current_fingerprint
            {
                reused += 1;
                old_pair.clone()
            } else {
                match parse_cert_pair(cert_id, cert_bytes.clone(), key_bytes.clone()) {
                    Ok(pair) => {
                        parsed_fresh += 1;
                        Arc::new(pair)
                    }
                    Err(err) => {
                        tracing::error!(
                            "SSL Parse Error for ID {}: certificate/key data invalid (cert_type={}, key_type={}): {}",
                            cert_id,
                            json_value_kind(c),
                            json_value_kind(k),
                            err
                        );
                        continue;
                    }
                }
            };

            if let Some(record) = records.get(&cert_id) {
                pair.apply_ocsp(record);
            }
            parsed += 1;
            new_cache.insert(cert_id, (current_fingerprint, pair.clone()));
            let pair = pair_view(&pair, cert_cfg.ocsp);

            if cert_cfg.is_default {
                default_pair = Some(pair.clone());
            }
            if fallback_pair.is_none() {
                fallback_pair = Some(pair.clone());
            }

            let mut names = certificate_names_from_der(pair.certified_key.cert.first());
            names.extend(
                cert_cfg
                    .dns_names
                    .iter()
                    .map(|name| name.trim())
                    .filter(|name| !name.is_empty())
                    .map(ToString::to_string),
            );

            for name in names {
                let name_low = name.to_lowercase();
                if name_low.starts_with("*.") {
                    new_wildcard.insert(name_low, pair.clone());
                } else {
                    new_exact.insert(name_low, pair.clone());
                }
            }
        }
        (parsed, reused, parsed_fresh)
    };

    // A bad replacement of the default certificate must not leave the listener
    // without a default while the previous material is still authorized and
    // unexpired. Explicit removal (certificate gone/disabled) still drops it.
    if default_pair.is_none()
        && let Some(old_default) = old_snapshot.default.as_ref()
        && global_authorized.contains(&cert_identity(old_default))
        && pair_unexpired(old_default, now)
    {
        default_pair = Some(old_default.clone());
        retained_last_good += 1;
        tracing::warn!(
            "SSL default certificate {} is temporarily invalid; retaining unexpired last-known-good material",
            old_default.id
        );
    }

    let mut new_snapshot = CertSnapshot {
        exact: new_exact,
        wildcard: new_wildcard,
        default: default_pair.or(fallback_pair),
        last_good: HashMap::new(),
        loaded_bindings: HashMap::new(),
    };

    // Phase 1: runtime names owned by a server HTTPS block are resolved from
    // that server's policy only. Remove the top-level SAN/dnsNames fallback so
    // display metadata can never stand in for real leaf coverage.
    for server in servers.iter().filter(|server| server.https.is_some()) {
        for name in crate::rpc::utils::server_runtime_names(server) {
            let name = name.to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            new_snapshot.exact.remove(&name);
            new_snapshot.wildcard.remove(&name);
        }
    }

    // Phase 2: bind each runtime name to the first configured certificate whose
    // actual leaf SAN covers it. One host may list several certificates for
    // different names; the old "first certificate for every name" mapping sent
    // the wrong certificate for every name after the first.
    //
    // An unstapled view is per hostname: shared certificates retain their
    // cached staple for other sites. Publish all views in one snapshot.
    for server in servers
        .iter()
        .filter(|server| server.is_on && server.https.is_some())
    {
        let Some(policy) = server
            .https
            .as_ref()
            .filter(|https| https.is_on)
            .and_then(|https| https.ssl_policy.as_ref())
            .filter(|policy| policy.is_on)
        else {
            continue;
        };

        let server_id = server.numeric_id();
        let mut candidates = Vec::new();
        let mut binding_authorized = HashSet::new();
        for cert_cfg in policy.certs.iter().filter(|cert| cert.is_on) {
            if let Some(identity) = cert_identity_from_config(cert_cfg) {
                binding_authorized.insert(identity);
            }
            if let Some(pair) = configured_pair(cert_cfg, &new_cache) {
                candidates.push(pair);
            }
        }

        for name in crate::rpc::utils::server_runtime_names(server) {
            let name = name.to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            let selected = if let Some(pair) =
                candidates.iter().find(|pair| pair_covers_name(pair, &name))
            {
                Some((pair.clone(), false))
            } else if let Some(old) = old_snapshot.last_good.get(&name) {
                match last_good_rejection(old, &binding_authorized, &global_authorized, now) {
                    None => {
                        if let Some(record) = records.get(&old.id) {
                            old.apply_ocsp(record);
                        }
                        retained_last_good += 1;
                        tracing::warn!(
                            "SSL binding {}: configured replacement is invalid; retaining unexpired last-known-good certificate {}",
                            name,
                            old.id
                        );
                        Some((old.clone(), true))
                    }
                    Some(reason) => {
                        dropped_last_good += 1;
                        tracing::warn!(
                            "SSL binding {}: dropping last-known-good certificate {}: {}",
                            name,
                            old.id,
                            reason
                        );
                        None
                    }
                }
            } else {
                tracing::warn!(
                    "SSL binding {}: no enabled certificate in the server policy has a leaf SAN covering this name",
                    name
                );
                None
            };

            if let Some((pair, retained_previous)) = selected {
                new_snapshot.last_good.insert(name.clone(), pair.clone());
                let view = pair_view(&pair, policy.ocsp);
                if name.starts_with("*.") {
                    new_snapshot.wildcard.insert(name.clone(), view);
                } else {
                    new_snapshot.exact.insert(name.clone(), view);
                }
                new_snapshot.loaded_bindings.insert(
                    name,
                    LoadedBinding {
                        server_id,
                        pair,
                        retained_previous,
                    },
                );
            }
        }
    }

    let default_present = new_snapshot.default.is_some();
    let mut cache_lock = cert_selector
        .cache
        .write()
        .unwrap_or_else(|e| e.into_inner());
    *cache_lock = new_cache;
    cert_selector.snapshot.store(Arc::new(new_snapshot));
    // Publish the handle only after the snapshot store so a concurrent status
    // read can never pair a new handle with an older published snapshot.
    *SSL_SELECTOR.write().unwrap_or_else(|e| e.into_inner()) = Some(cert_selector.clone());

    tracing::info!(
        "SSL Sync Result: {} certs processed (Reused: {}, Parsed: {}). Default Cert present: {}. Last-known-good retained: {}, dropped: {}",
        stats_parsed,
        stats_reused,
        stats_parsed_fresh,
        default_present,
        retained_last_good,
        dropped_last_good
    );
}

fn cert_identity_from_config(cert_cfg: &SSLCertConfig) -> Option<CertIdentity> {
    if cert_cfg.id > 0 {
        return Some(CertIdentity::Id(cert_cfg.id));
    }
    let cert_bytes = cert_value_to_bytes(cert_cfg.cert_data_json.as_ref()?)?;
    let key_bytes = cert_value_to_bytes(cert_cfg.key_data_json.as_ref()?)?;
    Some(CertIdentity::Material(fnv_hash64_bytes(&[
        &cert_bytes,
        &key_bytes,
    ])))
}

fn cert_identity(pair: &CertPair) -> CertIdentity {
    if pair.id > 0 {
        CertIdentity::Id(pair.id)
    } else {
        CertIdentity::Material(fnv_hash64_bytes(&[&pair.cert_bytes, &pair.key_bytes]))
    }
}

fn configured_pair(
    cert_cfg: &SSLCertConfig,
    cache: &HashMap<i64, (u64, Arc<CertPair>)>,
) -> Option<Arc<CertPair>> {
    if cert_cfg.id > 0 {
        return cache.get(&cert_cfg.id).map(|(_, pair)| pair.clone());
    }
    // Inline certificates receive synthetic IDs when collected; match their
    // actual material rather than the original non-positive ID.
    let cert_bytes = cert_value_to_bytes(cert_cfg.cert_data_json.as_ref()?)?;
    let key_bytes = cert_value_to_bytes(cert_cfg.key_data_json.as_ref()?)?;
    let fingerprint = fnv_hash64_bytes(&[&cert_bytes, &key_bytes]);
    cache
        .values()
        .find(|(candidate, _)| *candidate == fingerprint)
        .map(|(_, pair)| pair.clone())
}

/// Whether the certificate's actual leaf SANs cover `name`.
///
/// `name` may itself be a wildcard binding (`*.example.com`); a wildcard SAN
/// covers it just as it covers a concrete immediate subdomain.
fn pair_covers_name(pair: &CertPair, name: &str) -> bool {
    certificate_san_dns_names_from_der(pair.certified_key.cert.first())
        .iter()
        .any(|san| dns_name_covers(san, name))
}

fn pair_unexpired(pair: &CertPair, now: i64) -> bool {
    certificate_not_after(pair.certified_key.cert.first()).is_some_and(|not_after| not_after > now)
}

fn last_good_rejection(
    pair: &CertPair,
    binding_authorized: &HashSet<CertIdentity>,
    global_authorized: &HashSet<CertIdentity>,
    now: i64,
) -> Option<&'static str> {
    if !pair_unexpired(pair, now) {
        return Some("certificate expired");
    }
    let identity = cert_identity(pair);
    if !binding_authorized.contains(&identity) {
        return Some("server binding no longer authorizes this certificate");
    }
    if !global_authorized.contains(&identity) {
        return Some("certificate is no longer enabled in the top-level pool");
    }
    None
}

fn pair_view(pair: &Arc<CertPair>, ocsp_enabled: bool) -> Arc<CertPair> {
    if pair.ocsp_enabled == ocsp_enabled {
        return pair.clone();
    }
    let mut view = (**pair).clone();
    view.ocsp_enabled = ocsp_enabled;
    Arc::new(view)
}

fn normalize_dns_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// RFC 6125-style wildcard coverage: a SAN wildcard covers exactly one label.
fn dns_name_covers(san: &str, name: &str) -> bool {
    let san = normalize_dns_name(san);
    let name = normalize_dns_name(name);
    if san.is_empty() || name.is_empty() {
        return false;
    }
    if san == name {
        return true;
    }
    let Some(suffix) = san.strip_prefix("*.") else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    let Some(prefix) = name.strip_suffix(suffix) else {
        return false;
    };
    let Some(label) = prefix.strip_suffix('.') else {
        return false;
    };
    !label.is_empty() && !label.contains('.')
}

fn certificate_not_after(cert: Option<&CertificateDer<'static>>) -> Option<i64> {
    let cert = cert?;
    let (_, parsed) = X509Certificate::from_der(cert.as_ref()).ok()?;
    Some(parsed.validity().not_after.timestamp())
}

/// Leaf-only identity of loaded material: SHA-256 over the leaf certificate
/// DER, plus its notAfter as RFC 3339 (UTC, seconds). Never hashes the key.
fn loaded_leaf_identity(pair: &CertPair) -> (Option<String>, Option<String>) {
    let Some(cert) = pair.certified_key.cert.first() else {
        return (None, None);
    };
    let sha256 = hex::encode(Sha256::digest(cert.as_ref()));
    let not_after = certificate_not_after(Some(cert))
        .and_then(|timestamp| chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0))
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    (Some(sha256), not_after)
}

fn parse_cert_pair(
    cert_id: i64,
    cert_bytes: Vec<u8>,
    key_bytes: Vec<u8>,
) -> anyhow::Result<CertPair> {
    let cert_chain = parse_cert_chain(&cert_bytes)?;
    let key = parse_private_key(&key_bytes)?;
    let provider = default_crypto_provider();
    let certified_key = CertifiedKey::from_der(cert_chain, key, &provider)?;
    let certified_key = Arc::new(certified_key);

    Ok(CertPair {
        id: cert_id,
        certified_key,
        cert_bytes,
        key_bytes,
        ocsp: Arc::new(ArcSwap::from_pointee(OcspState::default())),
        ocsp_enabled: true,
    })
}

pub(crate) fn parse_cert_chain(bytes: &[u8]) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    if looks_like_pem(bytes) {
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut Cursor::new(bytes)).collect::<Result<_, _>>()?;
        anyhow::ensure!(!certs.is_empty(), "certificate chain is empty");
        return Ok(certs);
    }

    Ok(vec![CertificateDer::from(bytes.to_vec())])
}

pub(crate) fn parse_private_key(bytes: &[u8]) -> anyhow::Result<PrivateKeyDer<'static>> {
    if looks_like_pem(bytes) {
        return rustls_pemfile::private_key(&mut Cursor::new(bytes))?
            .ok_or_else(|| anyhow::anyhow!("private key is missing from PEM"));
    }

    PrivateKeyDer::try_from(bytes.to_vec()).map_err(|err| anyhow::anyhow!(err))
}

fn looks_like_pem(bytes: &[u8]) -> bool {
    bytes
        .windows(b"-----BEGIN ".len())
        .any(|w| w == b"-----BEGIN ")
}

fn certificate_names_from_der(cert: Option<&CertificateDer<'static>>) -> Vec<String> {
    let Some(cert) = cert else {
        return Vec::new();
    };
    let Ok((_, parsed)) = X509Certificate::from_der(cert.as_ref()) else {
        return Vec::new();
    };

    let mut names = Vec::new();
    if let Some(cn) = parsed
        .subject()
        .iter_common_name()
        .next()
        .and_then(|attr| attr.as_str().ok())
    {
        names.push(cn.to_string());
    }
    names.extend(san_dns_names(&parsed));
    names
}

fn certificate_san_dns_names_from_der(cert: Option<&CertificateDer<'static>>) -> Vec<String> {
    let Some(cert) = cert else {
        return Vec::new();
    };
    let Ok((_, parsed)) = X509Certificate::from_der(cert.as_ref()) else {
        return Vec::new();
    };
    san_dns_names(&parsed)
}

fn san_dns_names(parsed: &X509Certificate<'_>) -> Vec<String> {
    let Ok(Some(sans)) = parsed.subject_alternative_name() else {
        return Vec::new();
    };
    sans.value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(dns) => Some((*dns).to_string()),
            _ => None,
        })
        .collect()
}

fn cert_value_to_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(raw) => cert_string_to_bytes(raw),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let byte = item.as_u64()?;
                if byte > u8::MAX as u64 {
                    return None;
                }
                out.push(byte as u8);
            }
            if out.is_empty() { None } else { Some(out) }
        }
        Value::Object(map) => {
            for key in [
                "data", "value", "bytes", "pem", "content", "certData", "keyData",
            ] {
                if let Some(inner) = map.get(key)
                    && let Some(bytes) = cert_value_to_bytes(inner)
                {
                    return Some(bytes);
                }
            }
            None
        }
        _ => None,
    }
}

fn cert_string_to_bytes(raw: &str) -> Option<Vec<u8>> {
    let normalized = raw.replace("\\n", "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(encoded) = trimmed.strip_prefix("base64:") {
        return decode_base64(encoded.trim()).or_else(|| Some(trimmed.as_bytes().to_vec()));
    }

    if trimmed.contains("-----BEGIN ") {
        return Some(normalized.into_bytes());
    }

    decode_base64(trimmed).or_else(|| Some(normalized.as_bytes().to_vec()))
}

fn decode_base64(raw: &str) -> Option<Vec<u8>> {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    general_purpose::STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(compact.as_bytes()))
        .ok()
}

fn json_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn fnv_hash64_bytes(chunks: &[&[u8]]) -> u64 {
    const FNV_OFFSET: u64 = 14695981039346656037;
    const FNV_PRIME: u64 = 1099511628211;
    let mut hash = FNV_OFFSET;
    for chunk in chunks {
        for byte in *chunk {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config_models::{HTTPSConfig, ServerConfig, ServerNameConfig};

    const CERT_A_PEM: &str =
        include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem");
    const KEY_A_PEM: &str =
        include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem");
    const CERT_B_PEM: &str =
        include_str!("../pingora-main/pingora-proxy/tests/utils/conf/keys/server_rustls.crt");
    const KEY_B_PEM: &str =
        include_str!("../pingora-main/pingora-proxy/tests/utils/conf/keys/key.pem");
    /// Self-signed for `expired.example`, notAfter 2020-01-01, using `KEY_B_PEM`.
    const EXPIRED_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBgzCCASqgAwIBAgIUZlADkPyaxWFBiJLK9K5qckipKjkwCgYIKoZIzj0EAwIw
GjEYMBYGA1UEAwwPZXhwaXJlZC5leGFtcGxlMB4XDTE4MDEwMTAwMDAwMFoXDTIw
MDEwMTAwMDAwMFowGjEYMBYGA1UEAwwPZXhwaXJlZC5leGFtcGxlMFkwEwYHKoZI
zj0CAQYIKoZIzj0DAQcDQgAE2f/1Fm1HjySdokPq2T0F1xxol9nSEYQ+foFINeaW
Yk+FxMGpriJTBb8AGka87cWklw1ZqytfaT6pkureDbTkwqNOMEwwHQYDVR0OBBYE
FJ32ABVskJ0jeeyBqJI+43M/AoSUMA8GA1UdEwEB/wQFMAMBAf8wGgYDVR0RBBMw
EYIPZXhwaXJlZC5leGFtcGxlMAoGCCqGSM49BAMCA0cAMEQCIArJ0pJYprfdVAAr
Kmu5Hsi9tjAiS0fn42+HWSkEI++ZAiBngFv0kC32P+93qGFIIVO5WDKeOLgvig21
cUm2qSMECA==
-----END CERTIFICATE-----";

    fn cert(
        id: i64,
        cert_pem: &str,
        key_pem: &str,
        dns_names: &[&str],
        is_default: bool,
    ) -> SSLCertConfig {
        SSLCertConfig {
            ocsp: true,
            id,
            is_on: true,
            is_default,
            cert_data_json: Some(Value::String(cert_pem.to_string())),
            key_data_json: Some(Value::String(key_pem.to_string())),
            dns_names: dns_names.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    fn server(id: i64, names: &[&str], certs: Vec<SSLCertConfig>) -> Arc<ServerConfig> {
        Arc::new(ServerConfig {
            id: Some(id),
            is_on: true,
            server_names: names
                .iter()
                .map(|name| ServerNameConfig {
                    name: (*name).to_string(),
                    r#type: None,
                    sub_names: Vec::new(),
                })
                .collect(),
            https: Some(HTTPSConfig {
                is_on: true,
                listen: Vec::new(),
                ssl_policy: Some(SSLPolicyConfig {
                    ocsp: true,
                    id,
                    is_on: true,
                    certs,
                    http2_enabled: true,
                    min_version: String::new(),
                    hsts: None,
                }),
                supports_http3: None,
            }),
            ..Default::default()
        })
    }

    /// Reads the SNI binding without `find_pair_blocking`'s default-certificate
    /// fallback, so a missing map entry is distinguishable from a real match.
    fn bound_cert_pem(selector: &DynamicCertSelector, host: &str) -> Option<Vec<u8>> {
        let snapshot = selector.snapshot.load();
        if let Some(pair) = snapshot.exact.get(host) {
            return Some(pair.cert_bytes.clone());
        }
        let pos = host.find('.')?;
        snapshot
            .wildcard
            .get(&format!("*{}", &host[pos..]))
            .map(|pair| pair.cert_bytes.clone())
    }

    #[test]
    fn leaf_wildcard_covers_exactly_one_label() {
        assert!(dns_name_covers("*.openrusty.org", "a.openrusty.org"));
        assert!(dns_name_covers("*.openrusty.org", "*.openrusty.org"));
        assert!(dns_name_covers("openrusty.org", "openrusty.org"));
        assert!(!dns_name_covers("*.openrusty.org", "openrusty.org"));
        assert!(!dns_name_covers("*.openrusty.org", "a.b.openrusty.org"));
        assert!(!dns_name_covers("openrusty.org", "a.openrusty.org"));
    }

    #[tokio::test]
    async fn multi_cert_server_binds_each_name_to_its_covering_leaf() {
        let a = cert(101, CERT_A_PEM, KEY_A_PEM, &[], true);
        let b = cert(102, CERT_B_PEM, KEY_B_PEM, &[], false);
        let server = server(
            1,
            &["a.openrusty.org", "cat.com"],
            vec![a.clone(), b.clone()],
        );
        let selector = DynamicCertSelector::new();

        sync_certs_for_servers(&selector, &[a, b], &[server]).await;

        assert_eq!(
            bound_cert_pem(&selector, "a.openrusty.org").as_deref(),
            Some(CERT_A_PEM.as_bytes()),
            "the first configured certificate covers a.openrusty.org"
        );
        assert_eq!(
            bound_cert_pem(&selector, "cat.com").as_deref(),
            Some(CERT_B_PEM.as_bytes()),
            "cat.com must use the certificate whose leaf SAN covers it, not the first certificate"
        );
    }

    #[tokio::test]
    async fn wildcard_leaf_covers_concrete_and_wildcard_bindings() {
        let a = cert(103, CERT_A_PEM, KEY_A_PEM, &[], true);
        let server = server(
            1,
            &["api.openrusty.org", "*.openrusty.org"],
            vec![a.clone()],
        );
        let selector = DynamicCertSelector::new();

        sync_certs_for_servers(&selector, &[a], &[server]).await;

        assert_eq!(
            bound_cert_pem(&selector, "api.openrusty.org").as_deref(),
            Some(CERT_A_PEM.as_bytes()),
            "a wildcard leaf SAN covers an immediate subdomain"
        );
        assert_eq!(
            bound_cert_pem(&selector, "foo.openrusty.org").as_deref(),
            Some(CERT_A_PEM.as_bytes()),
            "a wildcard server binding is served by a wildcard leaf SAN"
        );
    }

    #[tokio::test]
    async fn display_dns_names_do_not_substitute_for_leaf_coverage() {
        let a = cert(104, CERT_A_PEM, KEY_A_PEM, &["uncovered.example"], true);
        let server = server(1, &["uncovered.example"], vec![a.clone()]);
        let selector = DynamicCertSelector::new();

        sync_certs_for_servers(&selector, &[a], &[server]).await;

        assert!(
            bound_cert_pem(&selector, "uncovered.example").is_none(),
            "dnsNames metadata must not bind a name the leaf SAN does not cover"
        );
    }

    #[tokio::test]
    async fn invalid_replacement_retains_unexpired_last_known_good_and_explicit_removal_drops_it() {
        let good = cert(110, CERT_A_PEM, KEY_A_PEM, &[], true);
        let selector = DynamicCertSelector::new();
        sync_certs_for_servers(
            &selector,
            std::slice::from_ref(&good),
            &[server(1, &["openrusty.org"], vec![good.clone()])],
        )
        .await;
        assert_eq!(
            bound_cert_pem(&selector, "openrusty.org").as_deref(),
            Some(CERT_A_PEM.as_bytes())
        );
        let leaf_sha256 = hex::encode(Sha256::digest(
            parse_cert_chain(CERT_A_PEM.as_bytes()).unwrap()[0].as_ref(),
        ));
        let loaded = selector.loaded_certificate_bindings();
        assert_eq!(loaded.as_array().unwrap().len(), 1);
        let binding = &loaded[0];
        assert_eq!(binding["serverId"], 1);
        assert_eq!(binding["hostname"], "openrusty.org");
        assert_eq!(binding["certificateId"], 110);
        assert_eq!(binding["sha256"], leaf_sha256);
        assert_eq!(binding["notAfter"], "2035-11-17T04:51:37Z");
        assert_eq!(binding["retainedPrevious"], false);
        assert!(
            binding.get("key").is_none() && binding.get("keyBytes").is_none(),
            "loaded observation must never expose private key material"
        );

        let invalid = cert(110, "not a certificate", KEY_A_PEM, &[], true);
        sync_certs_for_servers(
            &selector,
            std::slice::from_ref(&invalid),
            &[server(1, &["openrusty.org"], vec![invalid.clone()])],
        )
        .await;
        assert_eq!(
            bound_cert_pem(&selector, "openrusty.org").as_deref(),
            Some(CERT_A_PEM.as_bytes()),
            "an invalid replacement under the same certificate ID must keep the unexpired last-known-good binding"
        );
        assert!(
            selector.has_default_cert(),
            "an invalid replacement of the default certificate must not remove the default"
        );
        let loaded = selector.loaded_certificate_bindings();
        assert_eq!(
            loaded[0]["sha256"], leaf_sha256,
            "loaded observation must report the retained leaf, not the invalid desired replacement"
        );
        assert_eq!(loaded[0]["retainedPrevious"], true);

        sync_certs_for_servers(&selector, &[], &[server(1, &["openrusty.org"], vec![])]).await;
        assert!(
            bound_cert_pem(&selector, "openrusty.org").is_none(),
            "explicit removal must remove the last-known-good binding"
        );
        assert!(
            !selector.has_default_cert(),
            "explicit removal of the last certificate must remove the default"
        );
        assert!(
            selector
                .loaded_certificate_bindings()
                .as_array()
                .unwrap()
                .is_empty(),
            "removed bindings must disappear from loaded observation"
        );
    }

    #[tokio::test]
    async fn expired_last_known_good_is_not_retained() {
        let expired = cert(120, EXPIRED_CERT_PEM, KEY_B_PEM, &[], false);
        let selector = DynamicCertSelector::new();
        sync_certs_for_servers(
            &selector,
            std::slice::from_ref(&expired),
            &[server(1, &["expired.example"], vec![expired.clone()])],
        )
        .await;
        assert_eq!(
            bound_cert_pem(&selector, "expired.example").as_deref(),
            Some(EXPIRED_CERT_PEM.as_bytes())
        );

        let invalid = cert(120, "not a certificate", KEY_B_PEM, &[], false);
        sync_certs_for_servers(
            &selector,
            std::slice::from_ref(&invalid),
            &[server(1, &["expired.example"], vec![invalid.clone()])],
        )
        .await;
        assert!(
            bound_cert_pem(&selector, "expired.example").is_none(),
            "an expired certificate must never be retained as last-known-good"
        );
        assert!(!selector.has_default_cert());
    }

    #[tokio::test]
    async fn last_known_good_drops_when_binding_or_global_pool_revokes_it() {
        let good = cert(130, CERT_A_PEM, KEY_A_PEM, &[], true);

        let unbound = DynamicCertSelector::new();
        sync_certs_for_servers(
            &unbound,
            std::slice::from_ref(&good),
            &[server(1, &["openrusty.org"], vec![good.clone()])],
        )
        .await;
        sync_certs_for_servers(
            &unbound,
            std::slice::from_ref(&good),
            &[server(1, &["openrusty.org"], vec![])],
        )
        .await;
        assert!(
            bound_cert_pem(&unbound, "openrusty.org").is_none(),
            "removing the certificate from the binding stops the last-known-good binding"
        );

        let revoked = DynamicCertSelector::new();
        sync_certs_for_servers(
            &revoked,
            std::slice::from_ref(&good),
            &[server(1, &["openrusty.org"], vec![good.clone()])],
        )
        .await;
        sync_certs_for_servers(&revoked, &[], &[server(1, &["openrusty.org"], vec![good])]).await;
        assert!(
            bound_cert_pem(&revoked, "openrusty.org").is_none(),
            "removing the certificate from the top-level pool stops the last-known-good binding"
        );
    }
}
