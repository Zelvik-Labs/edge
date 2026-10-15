use arc_swap::ArcSwap;
use base64::{Engine as _, engine::general_purpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, RwLock};
use x509_parser::prelude::*;

use crate::config_models::{SSLCertConfig, SSLPolicyConfig};
use crate::tls_crypto::{default_crypto_provider, install_default_crypto_provider};

type CertCacheMap = Arc<RwLock<HashMap<i64, (String, Arc<CertPair>)>>>;

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
    let mut new_exact = HashMap::new();
    let mut new_wildcard = HashMap::new();
    let mut default_pair: Option<Arc<CertPair>> = None;
    let mut fallback_pair: Option<Arc<CertPair>> = None;
    let mut new_cache = HashMap::new();

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
                fnv_hash64_bytes(&[cert_bytes.as_slice(), key_bytes.as_slice()]).to_string();

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
            let pair = if cert_cfg.ocsp {
                pair
            } else {
                let mut view = (*pair).clone();
                view.ocsp_enabled = false;
                Arc::new(view)
            };

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

    let mut new_snapshot = CertSnapshot {
        exact: new_exact,
        wildcard: new_wildcard,
        default: default_pair.or(fallback_pair),
    };
    // An unstapled view is per hostname: shared certificates retain their
    // cached staple for other sites. Publish all views in one snapshot.
    for server in servers.iter().filter(|server| server.is_on) {
        let Some(policy) = server
            .https
            .as_ref()
            .filter(|https| https.is_on)
            .and_then(|https| https.ssl_policy.as_ref())
            .filter(|policy| policy.is_on)
        else {
            continue;
        };
        let pair = policy
            .certs
            .iter()
            .filter(|cert| cert.is_on)
            .find_map(|cert| {
                if cert.id > 0 {
                    return new_cache.get(&cert.id).map(|(_, pair)| pair.clone());
                }
                // Inline certificates receive synthetic IDs when collected; match
                // their actual material rather than the original zero ID.
                let cert_bytes = cert_value_to_bytes(cert.cert_data_json.as_ref()?)?;
                let key_bytes = cert_value_to_bytes(cert.key_data_json.as_ref()?)?;
                let fingerprint = fnv_hash64_bytes(&[&cert_bytes, &key_bytes]).to_string();
                new_cache
                    .values()
                    .find(|(candidate, _)| *candidate == fingerprint)
                    .map(|(_, pair)| pair.clone())
            });
        for name in crate::rpc::utils::server_runtime_names(server) {
            let name = name.to_ascii_lowercase();
            if let Some(pair) = &pair {
                let mut view = (**pair).clone();
                view.ocsp_enabled = policy.ocsp;
                if name.starts_with("*.") {
                    new_snapshot.wildcard.insert(name, Arc::new(view));
                } else {
                    new_snapshot.exact.insert(name, Arc::new(view));
                }
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

    tracing::info!(
        "SSL Sync Result: {} certs processed (Reused: {}, Parsed: {}). Default Cert present: {}",
        stats_parsed,
        stats_reused,
        stats_parsed_fresh,
        default_present
    );
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

    if let Ok(Some(sans)) = parsed.subject_alternative_name() {
        for name in &sans.value.general_names {
            if let GeneralName::DNSName(dns) = name {
                names.push((*dns).to_string());
            }
        }
    }

    names
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
