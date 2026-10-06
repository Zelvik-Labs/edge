use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock as Lazy;
use std::sync::RwLock;
use std::sync::atomic::{AtomicI8, Ordering};

static RUNTIME_RPC_ENDPOINTS: Lazy<RwLock<Option<Vec<String>>>> = Lazy::new(|| RwLock::new(None));
// Admin-configured package download bases pushed via runtime.api.update; takes
// precedence over the local yaml `update.endpoints` and rpc endpoints.
static RUNTIME_UPDATE_ENDPOINTS: Lazy<RwLock<Option<Vec<String>>>> =
    Lazy::new(|| RwLock::new(None));

static AUTO_ACCESS_LOG_QUEUE_CAPACITY: Lazy<usize> =
    Lazy::new(|| crate::memory_governor::MEMORY_GOVERNOR.access_log_queue_capacity());
static AUTO_ACCESS_LOG_BATCH_SIZE: Lazy<usize> =
    Lazy::new(|| crate::memory_governor::MEMORY_GOVERNOR.access_log_batch_size());

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApiConfig {
    #[serde(rename = "rpc.tls", default)]
    pub rpc_tls: RpcTlsConfig,
    #[serde(rename = "rpc.endpoints", default)]
    pub rpc_endpoints: Vec<String>,
    #[serde(rename = "rpc.disableUpdate", default)]
    pub rpc_disable_update: bool,
    #[serde(rename = "nodeId")]
    pub node_id: String,
    #[serde(rename = "secret")]
    pub secret: String,
    #[serde(rename = "accessLogPipeline", default)]
    pub access_log_pipeline: AccessLogPipelineConfig,
    #[serde(rename = "relay", default)]
    pub relay: RelayConfig,
    #[serde(rename = "kernelTuning", default)]
    pub kernel_tuning: KernelTuningConfig,
    #[serde(rename = "cache", default)]
    pub cache: LocalCacheConfig,
}

/// Update distribution endpoints carried by `runtime.api.update` pushes: the
/// admin management address(es) the node downloads upgrade packages from over
/// plain HTTP(S). Not a local yaml section — the control plane owns it (admin
/// platform_setting `edge.updateEndpoints`).
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct UpdateConfig {
    #[serde(rename = "endpoints", default)]
    pub endpoints: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct RpcTlsConfig {
    #[serde(rename = "caFile", default)]
    pub ca_file: Option<PathBuf>,
    #[serde(rename = "certFile", default)]
    pub cert_file: Option<PathBuf>,
    #[serde(rename = "keyFile", default)]
    pub key_file: Option<PathBuf>,
    #[serde(rename = "serverName", default)]
    pub server_name: Option<String>,
}

/// Node-local cache overrides. Every field is a tri-state: absent means
/// "follow whatever the control-plane cache policy pushes"; an explicit
/// bool forces the behavior on this node regardless of the pushed value.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct LocalCacheConfig {
    #[serde(rename = "sendfile", default)]
    pub sendfile: Option<bool>,
}

/// -1 = follow the pushed policy, 0 = force off, 1 = force on. Refreshed by
/// every `ApiConfig::load` so config edits take effect on the next policy
/// application without a restart of unrelated state.
static LOCAL_CACHE_SENDFILE: AtomicI8 = AtomicI8::new(-1);

/// Effective node-local sendfile override, if the operator set one.
pub fn local_cache_sendfile_override() -> Option<bool> {
    match LOCAL_CACHE_SENDFILE.load(Ordering::Relaxed) {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// `enabled` gates the whole sysctl pass. `params` carries operator-tuned
/// sysctl targets: keys matching a built-in plan entry replace its target,
/// unknown keys are appended as additional sysctls. `skip` removes built-in
/// keys outright. Applied at process start only — a pushed change is
/// persisted but stays unacknowledged until restart (see
/// `PreparedNodeApiPush::commit`).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct KernelTuningConfig {
    #[serde(rename = "enabled", default = "default_kernel_tuning_enabled")]
    pub enabled: bool,
    #[serde(rename = "params", default)]
    pub params: BTreeMap<String, String>,
    #[serde(rename = "skip", default)]
    pub skip: Vec<String>,
}

impl Default for KernelTuningConfig {
    fn default() -> Self {
        Self {
            enabled: default_kernel_tuning_enabled(),
            params: BTreeMap::new(),
            skip: Vec::new(),
        }
    }
}

impl KernelTuningConfig {
    pub fn normalized(&self) -> Self {
        Self {
            enabled: self.enabled,
            params: self
                .params
                .iter()
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .collect(),
            skip: self
                .skip
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RelayConfig {
    #[serde(rename = "zeroCopy", default)]
    pub zero_copy: bool,
    #[serde(
        rename = "tcpKeepaliveIdleSecs",
        default = "default_tcp_keepalive_idle_secs"
    )]
    pub tcp_keepalive_idle_secs: u32,
    #[serde(
        rename = "tcpKeepaliveIntervalSecs",
        default = "default_tcp_keepalive_interval_secs"
    )]
    pub tcp_keepalive_interval_secs: u32,
    #[serde(
        rename = "tcpKeepaliveProbes",
        default = "default_tcp_keepalive_probes"
    )]
    pub tcp_keepalive_probes: u32,
}

fn default_tcp_keepalive_idle_secs() -> u32 {
    60
}

fn default_tcp_keepalive_interval_secs() -> u32 {
    15
}

fn default_tcp_keepalive_probes() -> u32 {
    4
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            zero_copy: false,
            tcp_keepalive_idle_secs: default_tcp_keepalive_idle_secs(),
            tcp_keepalive_interval_secs: default_tcp_keepalive_interval_secs(),
            tcp_keepalive_probes: default_tcp_keepalive_probes(),
        }
    }
}

impl RelayConfig {
    pub fn normalized(&self) -> Self {
        Self {
            zero_copy: self.zero_copy,
            tcp_keepalive_idle_secs: self.tcp_keepalive_idle_secs.max(1),
            tcp_keepalive_interval_secs: self.tcp_keepalive_interval_secs.max(1),
            tcp_keepalive_probes: self.tcp_keepalive_probes.max(1),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct AccessLogPipelineConfig {
    #[serde(
        rename = "queueCapacity",
        default = "default_access_log_queue_capacity"
    )]
    pub queue_capacity: usize,
    #[serde(rename = "batchSize", default = "default_access_log_batch_size")]
    pub batch_size: usize,
    #[serde(
        rename = "flushIntervalMs",
        default = "default_access_log_flush_interval_ms"
    )]
    pub flush_interval_ms: u64,
    #[serde(
        rename = "uploadConcurrency",
        default = "default_access_log_upload_concurrency"
    )]
    pub upload_concurrency: usize,
    #[serde(rename = "retryQueueCapacity", default)]
    pub retry_queue_capacity: usize,
    #[serde(rename = "targetChunkBytes", default)]
    pub target_chunk_bytes: usize,
    #[serde(
        rename = "requestTimeoutMs",
        default = "default_access_log_request_timeout_ms"
    )]
    pub request_timeout_ms: u64,
    #[serde(
        rename = "warningIntervalMs",
        default = "default_access_log_warning_interval_ms"
    )]
    pub warning_interval_ms: u64,
}

impl Default for AccessLogPipelineConfig {
    fn default() -> Self {
        Self {
            queue_capacity: default_access_log_queue_capacity(),
            batch_size: default_access_log_batch_size(),
            flush_interval_ms: default_access_log_flush_interval_ms(),
            upload_concurrency: default_access_log_upload_concurrency(),
            retry_queue_capacity: 0,
            target_chunk_bytes: 0,
            request_timeout_ms: default_access_log_request_timeout_ms(),
            warning_interval_ms: default_access_log_warning_interval_ms(),
        }
    }
}

const MAX_ACCESS_LOG_UPLOAD_CONCURRENCY: usize = 32;

impl AccessLogPipelineConfig {
    pub fn normalized(&self) -> Self {
        let batch_size = self.batch_size.max(1);
        let queue_capacity = self.queue_capacity.max(batch_size);
        let upload_concurrency = if self.upload_concurrency == 0 {
            auto_access_log_upload_concurrency(queue_capacity, batch_size)
        } else {
            self.upload_concurrency
        }
        .clamp(1, MAX_ACCESS_LOG_UPLOAD_CONCURRENCY);
        Self {
            queue_capacity,
            batch_size,
            flush_interval_ms: self.flush_interval_ms.max(100),
            upload_concurrency,
            retry_queue_capacity: if self.retry_queue_capacity == 0 {
                batch_size
                    .saturating_mul(upload_concurrency)
                    .saturating_mul(10)
            } else {
                self.retry_queue_capacity.max(batch_size)
            },
            target_chunk_bytes: self.target_chunk_bytes,
            request_timeout_ms: self.request_timeout_ms.max(1000),
            warning_interval_ms: self.warning_interval_ms.max(1000),
        }
    }

    pub fn batch_queue_capacity(&self) -> usize {
        let batch_size = self.batch_size.max(1);
        ceil_div(self.queue_capacity.max(batch_size), batch_size)
            .max(self.upload_concurrency.saturating_mul(2))
            .max(1)
    }
}

fn ceil_div(value: usize, divisor: usize) -> usize {
    value.saturating_add(divisor.saturating_sub(1)) / divisor.max(1)
}

fn auto_access_log_upload_concurrency(queue_capacity: usize, batch_size: usize) -> usize {
    let queue_batches = ceil_div(queue_capacity, batch_size).max(1);
    let cpu_parallelism = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    queue_batches
        .min(cpu_parallelism.saturating_mul(2).max(1))
        .clamp(1, MAX_ACCESS_LOG_UPLOAD_CONCURRENCY)
}

fn default_access_log_queue_capacity() -> usize {
    *AUTO_ACCESS_LOG_QUEUE_CAPACITY
}

fn default_access_log_batch_size() -> usize {
    *AUTO_ACCESS_LOG_BATCH_SIZE
}

fn default_access_log_flush_interval_ms() -> u64 {
    5_000
}

fn default_access_log_upload_concurrency() -> usize {
    0
}

fn default_access_log_request_timeout_ms() -> u64 {
    30_000
}

fn default_access_log_warning_interval_ms() -> u64 {
    5_000
}

fn default_kernel_tuning_enabled() -> bool {
    true
}

impl ApiConfig {
    pub fn default_paths() -> Vec<PathBuf> {
        crate::paths::NodePaths::current().api_config_candidates()
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        Lazy::force(&AUTO_ACCESS_LOG_QUEUE_CAPACITY);
        Lazy::force(&AUTO_ACCESS_LOG_BATCH_SIZE);
        let content = fs::read_to_string(&path)?;
        let mut config: ApiConfig = serde_yaml::from_str(&content)?;
        let override_path = crate::paths::NodePaths::current()
            .config_dir()
            .join("control-plane-runtime.json");
        if override_path.exists() {
            let pushed: NodeApiPush = serde_json::from_slice(&fs::read(&override_path)?)?;
            // The pushed update base never enters ApiConfig itself; it only
            // feeds the runtime override consulted by package downloads.
            Self::set_runtime_update_endpoints(
                pushed
                    .update
                    .as_ref()
                    .map(|update| update.endpoints.clone())
                    .unwrap_or_default(),
            );
            pushed.merge_into(&mut config);
        }
        STARTUP_PIPELINE.get_or_init(|| config.access_log_pipeline.normalized());
        STARTUP_KERNEL_TUNING.get_or_init(|| config.kernel_tuning.normalized());
        config.rpc_endpoints = validate_rpc_endpoints(config.rpc_endpoints)?;
        if config.rpc_endpoints.is_empty() {
            anyhow::bail!("no valid 'rpc.endpoints' in {:?}", path.as_ref());
        }
        if config.node_id.is_empty() {
            anyhow::bail!("'nodeId' required in {:?}", path.as_ref());
        }
        if config.secret.is_empty() {
            anyhow::bail!("'secret' required in {:?}", path.as_ref());
        }
        Self::set_runtime_rpc_endpoints(config.rpc_endpoints.clone())?;
        LOCAL_CACHE_SENDFILE.store(
            match config.cache.sendfile {
                Some(true) => 1,
                Some(false) => 0,
                None => -1,
            },
            Ordering::Relaxed,
        );
        Ok(config)
    }

    pub fn load_default() -> anyhow::Result<Self> {
        let node_paths = crate::paths::NodePaths::current();
        let canonical = node_paths.api_config_file();
        let paths = node_paths.api_config_candidates();
        for path in &paths {
            if path.exists() {
                if path != &canonical {
                    tracing::warn!(
                        "Loading API config from legacy path {}. Please migrate it to {}.",
                        path.display(),
                        canonical.display()
                    );
                } else {
                    tracing::info!("Loading config from: {}", path.display());
                }
                return Self::load(path);
            }
        }
        anyhow::bail!("no config file found in default paths: {:?}", paths)
    }

    pub fn write_default(&self) -> anyhow::Result<()> {
        let node_paths = crate::paths::NodePaths::current();
        fs::create_dir_all(node_paths.config_dir())?;
        let target = node_paths.api_config_file();
        let content = serde_yaml::to_string(self)?;
        fs::write(target, content)?;
        Ok(())
    }

    pub fn effective_rpc_endpoints(&self) -> Vec<String> {
        RUNTIME_RPC_ENDPOINTS
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|endpoints| !endpoints.is_empty())
            .unwrap_or_else(|| self.rpc_endpoints.clone())
    }

    /// HTTP(S) base URLs serving node upgrade packages. The admin pushes the
    /// download base via `runtime.api.update` over the RPC channel (persisted
    /// to control-plane-runtime.json); without a push the node derives the
    /// address from `rpc.endpoints`, which is correct whenever the same admin
    /// origin fronts both the gRPC channel and the package download endpoint.
    pub fn effective_update_endpoints(&self) -> Vec<String> {
        RUNTIME_UPDATE_ENDPOINTS
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|endpoints| !endpoints.is_empty())
            .unwrap_or_else(|| self.effective_rpc_endpoints())
    }

    pub fn set_runtime_update_endpoints(endpoints: Vec<String>) {
        let endpoints = validate_rpc_endpoints(endpoints).unwrap_or_default();
        if let Ok(mut guard) = RUNTIME_UPDATE_ENDPOINTS.write() {
            *guard = (!endpoints.is_empty()).then_some(endpoints);
        }
    }

    pub fn set_runtime_rpc_endpoints(endpoints: Vec<String>) -> anyhow::Result<()> {
        let endpoints = validate_rpc_endpoints(endpoints)?;
        if endpoints.is_empty() {
            anyhow::bail!("no valid RPC endpoints supplied");
        }
        if let Ok(mut guard) = RUNTIME_RPC_ENDPOINTS.write() {
            *guard = Some(endpoints);
        }
        Ok(())
    }
}

pub fn validate_rpc_endpoint(endpoint: &str) -> anyhow::Result<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        anyhow::bail!("RPC endpoint is empty");
    }
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid RPC endpoint {endpoint:?}: {err}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or_else(|| anyhow::anyhow!("RPC endpoint {endpoint:?} has no scheme"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("RPC endpoint {endpoint:?} has no host"))?;
    if uri
        .path_and_query()
        .is_some_and(|value| value.as_str() != "/")
    {
        anyhow::bail!("RPC endpoint {endpoint:?} must not contain a path or query");
    }
    if authority.as_str().contains('@') {
        anyhow::bail!("RPC endpoint {endpoint:?} must not contain userinfo");
    }
    if !matches!(scheme, "http" | "https") {
        anyhow::bail!("RPC endpoint {endpoint:?} uses unsupported scheme {scheme:?}");
    }
    // Plain HTTP/2 gRPC remains supported for legacy control planes. Deployments
    // should protect remote plaintext endpoints with a private network or tunnel.
    Ok(endpoint.trim_end_matches('/').to_string())
}

pub fn validate_rpc_endpoints(endpoints: Vec<String>) -> anyhow::Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let endpoint = validate_rpc_endpoint(&endpoint)?;
        if !normalized.contains(&endpoint) {
            normalized.push(endpoint);
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_access_log_pipeline_uses_auto_upload_concurrency() {
        let normalized = AccessLogPipelineConfig::default().normalized();

        assert!(normalized.upload_concurrency > 1);
        assert!(normalized.upload_concurrency <= MAX_ACCESS_LOG_UPLOAD_CONCURRENCY);
        assert_eq!(
            normalized.queue_capacity,
            crate::memory_governor::MEMORY_GOVERNOR
                .access_log_queue_capacity()
                .max(crate::memory_governor::MEMORY_GOVERNOR.access_log_batch_size())
        );
        assert_eq!(
            normalized.batch_queue_capacity(),
            ceil_div(normalized.queue_capacity, normalized.batch_size)
                .max(normalized.upload_concurrency * 2)
        );
    }

    #[test]
    fn explicit_access_log_upload_concurrency_is_respected() {
        let config = AccessLogPipelineConfig {
            upload_concurrency: 3,
            ..Default::default()
        }
        .normalized();

        assert_eq!(config.upload_concurrency, 3);
    }

    #[test]
    fn batch_queue_capacity_covers_main_queue_batches() {
        let config = AccessLogPipelineConfig {
            queue_capacity: 250_001,
            batch_size: 10_000,
            upload_concurrency: 2,
            ..Default::default()
        }
        .normalized();

        assert_eq!(config.batch_queue_capacity(), 26);
    }

    #[test]
    fn relay_config_defaults_to_stable_copy_path() {
        let config = RelayConfig::default().normalized();

        assert!(!config.zero_copy);
        assert_eq!(config.tcp_keepalive_idle_secs, 60);
        assert_eq!(config.tcp_keepalive_interval_secs, 15);
        assert_eq!(config.tcp_keepalive_probes, 4);
    }

    #[test]
    fn kernel_tuning_defaults_to_enabled_for_legacy_config() {
        let config: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
"#,
        )
        .unwrap();

        assert!(config.kernel_tuning.normalized().enabled);
    }

    #[test]
    fn cache_sendfile_defaults_to_following_pushed_policy() {
        let config: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
"#,
        )
        .unwrap();

        assert_eq!(config.cache.sendfile, None);
    }

    #[test]
    fn cache_sendfile_override_parses_bool() {
        let config: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
cache:
  sendfile: true
"#,
        )
        .unwrap();

        assert_eq!(config.cache.sendfile, Some(true));
    }

    #[test]
    fn kernel_tuning_can_be_disabled() {
        let config: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
kernelTuning:
  enabled: false
"#,
        )
        .unwrap();

        assert!(!config.kernel_tuning.normalized().enabled);
    }

    #[test]
    fn remote_plaintext_rpc_endpoint_remains_supported() {
        let endpoint = "http://36.134.185.75:8001";

        assert_eq!(validate_rpc_endpoint(endpoint).unwrap(), endpoint);
    }

    fn runtime_test_dir() -> (PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("cloudsidra-runtime-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let local = dir.join("api.yaml");
        fs::write(
            &local,
            "nodeId: '1'\nsecret: fixture\nrpc.endpoints: ['http://127.0.0.1:8001']\n",
        )
        .unwrap();
        (dir, local)
    }

    #[test]
    fn empty_pushes_keep_stable_auto_pipeline_defaults() {
        let (dir, local) = runtime_test_dir();
        let push = NodeApiPush::default();
        let first = push
            .prepare_at(&local, &dir)
            .unwrap()
            .config
            .access_log_pipeline
            .normalized();
        let second = push
            .prepare_at(&local, &dir)
            .unwrap()
            .config
            .access_log_pipeline
            .normalized();
        assert_eq!(first, second);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_push_after_explicit_pipeline_override_restores_auto_defaults() {
        let (dir, local) = runtime_test_dir();
        let push = NodeApiPush {
            access_log_pipeline: Some(AccessLogPipelineConfig {
                queue_capacity: 12_345,
                batch_size: 678,
                ..Default::default()
            }),
            ..Default::default()
        };
        let overridden = push
            .prepare_at(&local, &dir)
            .unwrap()
            .config
            .access_log_pipeline
            .clone();
        assert_eq!(overridden.queue_capacity, 12_345);
        assert_eq!(overridden.batch_size, 678);

        let restored = NodeApiPush::default()
            .prepare_at(&local, &dir)
            .unwrap()
            .config
            .access_log_pipeline
            .normalized();
        let local_config: ApiConfig = serde_yaml::from_slice(&fs::read(&local).unwrap()).unwrap();
        assert_eq!(restored, local_config.access_log_pipeline.normalized());
        assert_ne!(restored, overridden.normalized());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn distinct_startup_pipeline_configs_compare_unequal() {
        let a: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
accessLogPipeline:
  queueCapacity: 4096
  batchSize: 256
"#,
        )
        .unwrap();
        let b: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com
nodeId: "1"
secret: "secret"
accessLogPipeline:
  queueCapacity: 8192
  batchSize: 256
"#,
        )
        .unwrap();

        assert_ne!(
            a.access_log_pipeline.normalized(),
            b.access_log_pipeline.normalized()
        );
    }

    #[test]
    fn access_log_auto_defaults_do_not_drift() {
        let _: ApiConfig = serde_yaml::from_str(
            "nodeId: '1'\nsecret: fixture\nrpc.endpoints: ['http://127.0.0.1:8001']\n",
        )
        .unwrap();
        assert_eq!(
            default_access_log_queue_capacity(),
            default_access_log_queue_capacity()
        );
        assert_eq!(
            default_access_log_batch_size(),
            default_access_log_batch_size()
        );
        assert_eq!(
            AccessLogPipelineConfig::default().queue_capacity,
            default_access_log_queue_capacity()
        );
        assert_eq!(
            AccessLogPipelineConfig::default().batch_size,
            default_access_log_batch_size()
        );
    }
}

/// 平台 release 验签公钥：随 runtime.api 推送并经 control-plane-runtime.json
/// 持久化；节点用它校验 GetDesiredReleaseResponse.signature_digest（Ed25519）。
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReleaseSigningPush {
    #[serde(default)]
    pub algorithm: String,
    #[serde(rename = "publicKey", default)]
    pub public_key: String,
}

/// Persisted separately from local credentials so removing an override restores
/// the original operator configuration. Startup-only changes remain unacknowledged
/// until restart, rather than claiming that fixed-capacity queues were resized.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct NodeApiPush {
    #[serde(default)]
    pub relay: Option<RelayConfig>,
    #[serde(rename = "kernelTuning", default)]
    pub kernel_tuning: Option<KernelTuningConfig>,
    #[serde(default)]
    pub cache: Option<LocalCacheConfig>,
    #[serde(rename = "accessLogPipeline", default)]
    pub access_log_pipeline: Option<AccessLogPipelineConfig>,
    /// Admin-configured package update base (DB: platform_setting
    /// edge.updateEndpoints), pushed through the RPC channel.
    #[serde(default)]
    pub update: Option<UpdateConfig>,
    /// 平台签名公钥（hex ed25519）。快照 HMAC 已认证本段内容，公钥来源可信。
    #[serde(rename = "releaseSigning", default)]
    pub release_signing: Option<ReleaseSigningPush>,
}
static STARTUP_PIPELINE: std::sync::OnceLock<AccessLogPipelineConfig> = std::sync::OnceLock::new();
static STARTUP_KERNEL_TUNING: std::sync::OnceLock<KernelTuningConfig> = std::sync::OnceLock::new();

/// 已确认的 release 验签公钥：首随认证快照下发，之后持久化在
/// control-plane-runtime.json；每次 NodeApiPush 合并都刷新钉扎值。
static RELEASE_SIGNING_PUBKEY: std::sync::RwLock<Option<Vec<u8>>> = std::sync::RwLock::new(None);

/// install_release_signing_key pins the hex-ed25519 public key carried in a
/// NodeApiPush. Malformed material is ignored — verification then stays off
/// and the caller still sees an explicit "no key" warn in verify paths.
pub fn install_release_signing_key(push: &ReleaseSigningPush) {
    if !push.algorithm.eq_ignore_ascii_case("ed25519") {
        return;
    }
    match hex::decode(push.public_key.trim()) {
        Ok(key) if key.len() == 32 => {
            if let Ok(mut slot) = RELEASE_SIGNING_PUBKEY.write() {
                *slot = Some(key);
            }
        }
        _ => tracing::warn!("ignoring malformed releaseSigning.publicKey in pushed runtime"),
    }
}

/// release_signing_pubkey returns the pinned platform key, if any.
pub fn release_signing_pubkey() -> Option<Vec<u8>> {
    RELEASE_SIGNING_PUBKEY
        .read()
        .ok()
        .and_then(|key| key.clone())
}

/// clears the pinned release signing key (test-only: parallel test binaries
/// cannot rely on the global slot staying empty).
#[cfg(test)]
pub fn reset_release_signing_key() {
    if let Ok(mut slot) = RELEASE_SIGNING_PUBKEY.write() {
        *slot = None;
    }
}

/// Serializes every test that mutates the process-global pinned key slot —
/// lock in the test fn before install/reset, hold until done.
#[cfg(test)]
pub(crate) static RELEASE_SIGNING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// verify_release_signature checks `signature_digest` as an Ed25519 signature
/// over `release_id` against the pinned platform key.
/// 返回 Err 仅当公钥已知且签名不匹配——公钥未知（旧 API 未下发）时跳过并告警。
pub fn verify_release_signature(release_id: &str, digest: &[u8]) -> Result<(), tonic::Code> {
    use aws_lc_rs::signature::{ED25519, UnparsedPublicKey};
    let Some(key) = release_signing_pubkey() else {
        tracing::warn!("no release signing key pinned; skipping signature_digest verification");
        return Ok(());
    };
    if digest.is_empty() {
        return Ok(());
    }
    UnparsedPublicKey::new(&ED25519, key.as_slice())
        .verify(release_id.as_bytes(), digest)
        .map_err(|_| tonic::Code::DataLoss)
}

impl NodeApiPush {
    fn merge_into(&self, config: &mut ApiConfig) {
        if let Some(value) = &self.relay {
            config.relay = value.clone();
        }
        if let Some(value) = &self.kernel_tuning {
            config.kernel_tuning = value.clone();
        }
        if let Some(value) = &self.cache {
            config.cache = value.clone();
        }
        if let Some(value) = &self.access_log_pipeline {
            config.access_log_pipeline = value.clone();
        }
        if let Some(push) = &self.release_signing {
            install_release_signing_key(push);
        }
    }
    pub fn prepare(&self) -> anyhow::Result<PreparedNodeApiPush> {
        let paths = ApiConfig::default_paths();
        let path = paths
            .iter()
            .find(|path| path.exists())
            .ok_or_else(|| anyhow::anyhow!("local API configuration missing"))?;
        self.prepare_at(path, &crate::paths::NodePaths::current().config_dir())
    }
    fn prepare_at(&self, path: &Path, dir: &Path) -> anyhow::Result<PreparedNodeApiPush> {
        let mut config: ApiConfig = serde_yaml::from_slice(&fs::read(path)?)?;
        self.merge_into(&mut config);
        fs::create_dir_all(dir)?;
        let target = dir.join("control-plane-runtime.json");
        let temp = dir.join(format!(
            "control-plane-runtime.{}.tmp",
            uuid::Uuid::new_v4()
        ));
        fs::write(&temp, serde_json::to_vec(self)?)?;
        Ok(PreparedNodeApiPush {
            config,
            target,
            temp,
        })
    }
    pub fn activate(&self, config: &ApiConfig) {
        // 管理端下发的包下载基址即时生效（升级任务执行时才读取）。
        ApiConfig::set_runtime_update_endpoints(
            self.update
                .as_ref()
                .map(|update| update.endpoints.clone())
                .unwrap_or_default(),
        );
        crate::tcp_proxy::configure_relay_from_api(config);
        LOCAL_CACHE_SENDFILE.store(
            match config.cache.sendfile {
                Some(true) => 1,
                Some(false) => 0,
                None => -1,
            },
            Ordering::Relaxed,
        );
    }
}

/// The staging file is never read at startup. A failed XDP generation drops it.
pub struct PreparedNodeApiPush {
    pub config: ApiConfig,
    target: PathBuf,
    temp: PathBuf,
}
impl PreparedNodeApiPush {
    pub fn commit(&self) -> anyhow::Result<()> {
        fs::rename(&self.temp, &self.target)?;
        if STARTUP_PIPELINE
            .get()
            .is_some_and(|current| *current != self.config.access_log_pipeline.normalized())
            || STARTUP_KERNEL_TUNING
                .get()
                .is_some_and(|current| *current != self.config.kernel_tuning.normalized())
        {
            anyhow::bail!(
                "restart required for accessLogPipeline/kernelTuning; validated override persisted, generation not acknowledged"
            );
        }
        Ok(())
    }
}
impl Drop for PreparedNodeApiPush {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.temp)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!("runtime staging cleanup failed: {}", err);
        }
    }
}

#[cfg(test)]
mod pushed_runtime_tests {
    use super::*;
    #[test]
    fn failed_generation_never_publishes_its_staged_override() {
        let dir =
            std::env::temp_dir().join(format!("cloudsidra-runtime-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let local = dir.join("api.yaml");
        fs::write(
            &local,
            "nodeId: '1'\nsecret: fixture\nrpc.endpoints: ['http://127.0.0.1:8001']\n",
        )
        .unwrap();
        let push = NodeApiPush {
            relay: Some(RelayConfig {
                zero_copy: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        let prepared = push.prepare_at(&local, &dir).unwrap();
        assert!(prepared.config.relay.zero_copy);
        assert!(!dir.join("control-plane-runtime.json").exists());
        assert!(prepared.temp.exists());
        let temp = prepared.temp.clone();
        drop(prepared);
        assert!(!temp.exists());
        assert!(!dir.join("control-plane-runtime.json").exists());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod release_signing_tests {
    use super::*;
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};

    // 一次生成覆盖本模块全部用例：公钥十六进制 + 对 release_id 签名。
    fn seeded_keypair() -> (Ed25519KeyPair, Vec<u8>) {
        let doc = Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let pubkey = pair.public_key().as_ref().to_vec();
        (pair, pubkey)
    }

    // 全局钉扎槽单测内串行——并行 #[test] 会互相覆盖 RELEASE_SIGNING_PUBKEY。
    #[test]
    fn pinned_key_verifies_release_signature() {
        let _g = RELEASE_SIGNING_TEST_LOCK.lock().unwrap();
        let (pair, pubkey) = seeded_keypair();
        install_release_signing_key(&ReleaseSigningPush {
            algorithm: "ed25519".into(),
            public_key: hex::encode(&pubkey),
        });
        assert_eq!(release_signing_pubkey().as_deref(), Some(pubkey.as_slice()));

        let sig = pair.sign(b"rel-42").as_ref().to_vec();
        assert!(verify_release_signature("rel-42", &sig).is_ok());
        assert!(verify_release_signature("rel-43", &sig).is_err());
        // 空 digest = 无 release 可验，不误拒
        assert!(verify_release_signature("rel-42", &[]).is_ok());

        // 非法/非 ed25519 输入不破坏已钉扎的 key
        install_release_signing_key(&ReleaseSigningPush {
            algorithm: "ed25519".into(),
            public_key: "zz".into(),
        });
        install_release_signing_key(&ReleaseSigningPush {
            algorithm: "rsa".into(),
            public_key: hex::encode([0u8; 32]),
        });
        assert!(verify_release_signature("rel-42", &sig).is_ok());
    }
}

#[cfg(test)]
mod update_endpoint_tests {
    use super::*;

    // Single test for the whole lifecycle: RUNTIME_UPDATE_ENDPOINTS is a
    // process-global push override, so splitting into parallel tests would race.
    #[test]
    fn update_endpoints_follow_pushed_value_then_rpc_fallback() {
        let config: ApiConfig = serde_yaml::from_str(
            r#"
rpc.endpoints:
  - https://api.example.com:9090
nodeId: "1"
secret: "secret"
"#,
        )
        .unwrap();
        assert_eq!(
            config.effective_update_endpoints(),
            vec!["https://api.example.com:9090".to_string()]
        );

        let push = NodeApiPush {
            update: Some(UpdateConfig {
                endpoints: vec!["https://admin.example.com".to_string()],
            }),
            ..Default::default()
        };
        push.activate(&config);
        assert_eq!(
            config.effective_update_endpoints(),
            vec!["https://admin.example.com".to_string()]
        );

        ApiConfig::set_runtime_update_endpoints(Vec::new());
        assert_eq!(
            config.effective_update_endpoints(),
            vec!["https://api.example.com:9090".to_string()]
        );
    }
}
