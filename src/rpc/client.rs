#![allow(clippy::type_complexity, clippy::result_large_err)]

use crate::api_config::ApiConfig;
use crate::pb;
use tonic::codec::CompressionEncoding;

use arc_swap::ArcSwap;
use std::sync::Arc;
use std::sync::LazyLock as Lazy;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tonic::transport::channel::Change;
use tonic::transport::{Channel, Endpoint};

pub const RPC_MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
pub const RPC_TRANSFER_MESSAGE_BYTES: usize = 512 * 1024 * 1024;
pub const RPC_NODE_STREAM_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RPC_TCP_KEEPALIVE: Duration = Duration::from_secs(30);
const RPC_HTTP2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10 * 60);
const RPC_HTTP2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_PING_CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpcConnectionProfile {
    Default,
    NodeStream,
    PingProbe,
}

impl RpcConnectionProfile {
    fn connect_timeout(self) -> Duration {
        match self {
            Self::Default | Self::NodeStream => RPC_CONNECT_TIMEOUT,
            Self::PingProbe => RPC_PING_CONNECT_TIMEOUT,
        }
    }

    fn http2_keep_alive_interval(self) -> Option<Duration> {
        match self {
            // Keep ordinary unary channels conservative. The node stream uses
            // application-level heartbeats and must not trigger gRPC-Go HTTP/2
            // keepalive enforcement with client-side PING frames.
            Self::Default => Some(RPC_HTTP2_KEEPALIVE_INTERVAL),
            Self::NodeStream | Self::PingProbe => None,
        }
    }

    fn keep_alive_while_idle(self) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct RpcClient {
    channel: Channel,
    api_config: ApiConfig,
}

impl RpcClient {
    pub async fn new(api_config: &ApiConfig) -> anyhow::Result<Self> {
        let endpoints = api_config.effective_rpc_endpoints();
        Self::connect(api_config, &endpoints, RpcConnectionProfile::Default).await
    }

    /// Create a client for a stream, attempting all endpoints. If `force` is
    /// false, uses the exact list given; if true, re-reads from api_config.
    pub async fn new_with_endpoints(
        api_config: &ApiConfig,
        endpoints: &[String],
        force: bool,
    ) -> anyhow::Result<Self> {
        let list: Vec<String> = if force || endpoints.is_empty() {
            api_config.effective_rpc_endpoints()
        } else {
            endpoints.to_vec()
        };
        Self::connect(api_config, &list, RpcConnectionProfile::Default).await
    }

    pub async fn new_stream(api_config: &ApiConfig) -> anyhow::Result<Self> {
        let endpoints = api_config.effective_rpc_endpoints();
        Self::connect(api_config, &endpoints, RpcConnectionProfile::NodeStream).await
    }

    /// Create a node stream client with a stream-safe transport profile.
    ///
    /// The bidirectional node stream already sends `NodeStreamMessage { code:
    /// "ping" }` heartbeats. We intentionally avoid HTTP/2 keepalive PINGs on
    /// this channel because Go gRPC servers commonly enforce a minimum client
    /// ping interval and close streams that ping too aggressively.
    pub async fn new_stream_with_endpoints(
        api_config: &ApiConfig,
        endpoints: &[String],
        force: bool,
    ) -> anyhow::Result<Self> {
        let list: Vec<String> = if force || endpoints.is_empty() {
            api_config.effective_rpc_endpoints()
        } else {
            endpoints.to_vec()
        };
        Self::connect(api_config, &list, RpcConnectionProfile::NodeStream).await
    }

    async fn connect(
        api_config: &ApiConfig,
        endpoints: &[String],
        profile: RpcConnectionProfile,
    ) -> anyhow::Result<Self> {
        if endpoints.is_empty() {
            anyhow::bail!("No RPC endpoints configured");
        }
        let mut last_err = None;
        for api_endpoint in endpoints {
            let endpoint = match crate::api_config::validate_rpc_endpoint(api_endpoint) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    last_err = Some(err);
                    continue;
                }
            };
            let endpoint = match Channel::from_shared(endpoint) {
                Ok(endpoint) => match endpoint.user_agent("grpc-go/1.0") {
                    Ok(endpoint) => endpoint,
                    Err(e) => {
                        last_err = Some(anyhow::anyhow!(
                            "Invalid user-agent for {}: {}",
                            api_endpoint,
                            e
                        ));
                        continue;
                    }
                },
                Err(e) => {
                    last_err = Some(anyhow::anyhow!("Invalid URI {}: {}", api_endpoint, e));
                    continue;
                }
            };

            let endpoint = Self::configure_tls(endpoint, api_config)?;
            let channel = Self::configure_endpoint(endpoint, profile).connect().await;

            match channel {
                Ok(channel) => {
                    return Ok(Self {
                        channel,
                        api_config: api_config.clone(),
                    });
                }
                Err(err) => {
                    last_err = Some(anyhow::anyhow!(
                        "Failed to connect to {}: {}",
                        api_endpoint,
                        err
                    ));
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("Unable to connect to any RPC endpoint")))
    }

    /// Probe one candidate endpoint without changing any membership state.
    /// Configuration errors, connection failures and authentication failures
    /// are all returned to the caller; callers that only need a boolean can
    /// use `.is_ok()`. Authentication errors are never converted into a
    /// different-endpoint fallback here.
    pub async fn probe_endpoint(
        api_config: &ApiConfig,
        endpoint: &str,
    ) -> Result<(), tonic::Status> {
        let endpoint = crate::api_config::validate_rpc_endpoint(endpoint)
            .map_err(|err| tonic::Status::invalid_argument(err.to_string()))?;
        let endpoint = Channel::from_shared(endpoint)
            .map_err(|err| tonic::Status::invalid_argument(format!("invalid URI: {err}")))?
            .user_agent("grpc-go/1.0")
            .map_err(|err| tonic::Status::invalid_argument(format!("invalid user-agent: {err}")))?;

        let endpoint = Self::configure_tls(endpoint, api_config)
            .map_err(|err| tonic::Status::invalid_argument(err.to_string()))?;
        let channel = Self::configure_endpoint(endpoint, RpcConnectionProfile::PingProbe)
            .connect()
            .await
            .map_err(|err| tonic::Status::unavailable(format!("connect failed: {err}")))?;

        let mut service = pb::ping_service_client::PingServiceClient::with_interceptor(
            channel,
            Self::interceptor(api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES);

        service.ping(pb::PingRequest {}).await.map(|_| ())
    }

    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }

    fn configure_tls(endpoint: Endpoint, api: &ApiConfig) -> anyhow::Result<Endpoint> {
        let cfg = &api.rpc_tls;
        if endpoint.uri().scheme_str() != Some("https") {
            if cfg.cert_file.is_some() || cfg.key_file.is_some() || cfg.ca_file.is_some() {
                anyhow::bail!("RPC TLS credentials require an https endpoint");
            }
            return Ok(endpoint);
        }
        use tonic::transport::{Certificate, ClientTlsConfig, Identity};
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(path) = &cfg.ca_file {
            tls = tls.ca_certificate(Certificate::from_pem(std::fs::read(path)?));
        }
        if let Some(name) = &cfg.server_name {
            tls = tls.domain_name(name);
        }
        match (&cfg.cert_file, &cfg.key_file) {
            (Some(cert), Some(key)) => {
                tls = tls.identity(Identity::from_pem(
                    std::fs::read(cert)?,
                    std::fs::read(key)?,
                ));
            }
            (None, None) => {}
            _ => anyhow::bail!("RPC client TLS requires both certFile and keyFile"),
        }
        Ok(endpoint.tls_config(tls)?)
    }

    fn configure_endpoint(endpoint: Endpoint, profile: RpcConnectionProfile) -> Endpoint {
        let endpoint = endpoint
            .connect_timeout(profile.connect_timeout())
            .tcp_keepalive(Some(RPC_TCP_KEEPALIVE));

        if let Some(interval) = profile.http2_keep_alive_interval() {
            endpoint
                .http2_keep_alive_interval(interval)
                .keep_alive_timeout(RPC_HTTP2_KEEPALIVE_TIMEOUT)
                .keep_alive_while_idle(profile.keep_alive_while_idle())
        } else {
            endpoint
        }
    }

    /// The credential interceptor of every control-plane client stub: the shared
    /// suite injects the four `zelvik-*` metadata keys and re-mints the `zvc1`
    /// credential for each RPC. The draft-period `nodeid`/`nodeId`/`type`/
    /// `token` keys are deleted with no dual read.
    fn interceptor(api_config: &ApiConfig) -> zelvik_suite::channel::CredentialInterceptor {
        api_config
            .credential_interceptor()
            .expect("control-plane identity must be valid before an RPC client is built")
    }

    #[cfg(feature = "node-control")]
    pub fn node_control_service(
        &self,
    ) -> crate::node_control::proto::node_control_client::NodeControlClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        crate::node_control::proto::node_control_client::NodeControlClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_service(
        &self,
    ) -> pb::node_service_client::NodeServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_service_client::NodeServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_service_with_type(
        &self,
    ) -> pb::node_service_client::NodeServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_service_client::NodeServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_service_plain(
        &self,
    ) -> pb::node_service_client::NodeServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_service_client::NodeServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .max_decoding_message_size(RPC_NODE_STREAM_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_NODE_STREAM_MESSAGE_BYTES)
    }

    pub fn telemetry_service(
        &self,
    ) -> pb::telemetry_service_client::TelemetryServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::telemetry_service_client::TelemetryServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }
    pub fn server_service(
        &self,
    ) -> pb::server_service_client::ServerServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_service_client::ServerServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn server_service_with_type(
        &self,
    ) -> pb::server_service_client::ServerServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_service_client::ServerServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_task_service(
        &self,
    ) -> pb::node_task_service_client::NodeTaskServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_task_service_client::NodeTaskServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn user_service(
        &self,
    ) -> pb::user_service_client::UserServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::user_service_client::UserServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_value_service(
        &self,
    ) -> pb::node_value_service_client::NodeValueServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_value_service_client::NodeValueServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_value_service_with_type(
        &self,
    ) -> pb::node_value_service_client::NodeValueServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_value_service_client::NodeValueServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn node_log_service(
        &self,
    ) -> pb::node_log_service_client::NodeLogServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::node_log_service_client::NodeLogServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn bandwidth_stat_service(
        &self,
    ) -> pb::server_bandwidth_stat_service_client::ServerBandwidthStatServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_bandwidth_stat_service_client::ServerBandwidthStatServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn daily_stat_service(
        &self,
    ) -> pb::server_daily_stat_service_client::ServerDailyStatServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_daily_stat_service_client::ServerDailyStatServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn metric_stat_service(
        &self,
    ) -> pb::metric_stat_service_client::MetricStatServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::metric_stat_service_client::MetricStatServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ip_item_service(
        &self,
    ) -> pb::ip_item_service_client::IpItemServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ip_item_service_client::IpItemServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ip_item_service_with_type(
        &self,
    ) -> pb::ip_item_service_client::IpItemServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ip_item_service_client::IpItemServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ip_list_service(
        &self,
    ) -> pb::ip_list_service_client::IpListServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ip_list_service_client::IpListServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ip_list_service_with_type(
        &self,
    ) -> pb::ip_list_service_client::IpListServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ip_list_service_client::IpListServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn deleted_content_service(
        &self,
    ) -> pb::server_deleted_content_service_client::ServerDeletedContentServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_deleted_content_service_client::ServerDeletedContentServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn api_node_service(
        &self,
    ) -> pb::api_node_service_client::ApiNodeServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::api_node_service_client::ApiNodeServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn firewall_service(
        &self,
    ) -> pb::firewall_service_client::FirewallServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::firewall_service_client::FirewallServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn acme_service(
        &self,
    ) -> pb::acme_authentication_service_client::AcmeAuthenticationServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::acme_authentication_service_client::AcmeAuthenticationServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ip_library_artifact_service(
        &self,
    ) -> pb::ip_library_artifact_service_client::IpLibraryArtifactServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ip_library_artifact_service_client::IpLibraryArtifactServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_TRANSFER_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_TRANSFER_MESSAGE_BYTES)
    }

    pub fn file_chunk_service(
        &self,
    ) -> pb::file_chunk_service_client::FileChunkServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::file_chunk_service_client::FileChunkServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_TRANSFER_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_TRANSFER_MESSAGE_BYTES)
    }

    pub fn ssl_cert_service(
        &self,
    ) -> pb::ssl_cert_service_client::SslCertServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ssl_cert_service_client::SslCertServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn cache_task_service(
        &self,
    ) -> pb::http_cache_task_key_service_client::HttpCacheTaskKeyServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::http_cache_task_key_service_client::HttpCacheTaskKeyServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn ping_service(
        &self,
    ) -> pb::ping_service_client::PingServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::ping_service_client::PingServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn plan_service(
        &self,
    ) -> pb::plan_service_client::PlanServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::plan_service_client::PlanServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn updating_server_list_service(
        &self,
    ) -> pb::updating_server_list_service_client::UpdatingServerListServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::updating_server_list_service_client::UpdatingServerListServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn authority_key_service(
        &self,
    ) -> pb::authority_key_service_client::AuthorityKeyServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::authority_key_service_client::AuthorityKeyServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn client_agent_ip_service(
        &self,
    ) -> pb::client_agent_ip_service_client::ClientAgentIpServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::client_agent_ip_service_client::ClientAgentIpServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn file_service(
        &self,
    ) -> pb::file_service_client::FileServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::file_service_client::FileServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn script_service(
        &self,
    ) -> pb::script_service_client::ScriptServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::script_service_client::ScriptServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    /*pub fn index_service(&self) -> pb::index_service_client::IndexServiceClient<tonic::service::interceptor::InterceptedService<Channel, impl FnMut(Request<()>) -> Result<Request<()>, Status> + Clone + Send + 'static>> {
        pb::index_service_client::IndexServiceClient::with_interceptor(self.channel.clone(), Self::interceptor(&self.api_config, true))
    }*/

    pub fn server_event_service(
        &self,
    ) -> pb::server_event_service_client::ServerEventServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_event_service_client::ServerEventServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }

    pub fn server_top_ip_stat_service(
        &self,
    ) -> pb::server_top_ip_stat_service_client::ServerTopIpStatServiceClient<
        tonic::service::interceptor::InterceptedService<
            Channel,
            zelvik_suite::channel::CredentialInterceptor,
        >,
    > {
        pb::server_top_ip_stat_service_client::ServerTopIpStatServiceClient::with_interceptor(
            self.channel.clone(),
            Self::interceptor(&self.api_config),
        )
        .send_compressed(CompressionEncoding::Gzip)
        .accept_compressed(CompressionEncoding::Gzip)
        .max_decoding_message_size(RPC_MAX_MESSAGE_BYTES)
        .max_encoding_message_size(RPC_MAX_MESSAGE_BYTES)
    }
}

/// A shared, persistent RPC client that mirrors Go's SharedRPC() singleton architecture.
///
/// Uses tonic's `balance_channel` to create a single Channel that load-balances across
/// multiple endpoints (p2c algorithm — power-of-two-choices, more advanced than Go's
/// simple random selection). All 20+ syncers reuse this one Channel via HTTP/2 multiplexing.
///
/// Endpoints can be dynamically added/removed via `update_endpoints()` (mirrors Go's
/// `UpdateConfig()`), without rebuilding the Channel — just like Go's `pickConn()` swap.
static SHARED_RPC: Lazy<ArcSwap<Option<SharedRpcState>>> =
    Lazy::new(|| ArcSwap::from_pointee(None));

struct SharedRpcState {
    channel: Channel,
    endpoint_tx: Sender<Change<String, Endpoint>>,
    current_endpoints: Vec<String>,
}

pub struct SharedRpcClient {
    channel: Channel,
    api_config: ApiConfig,
}

impl SharedRpcClient {
    /// Drop the process-global channel — each test's runtime owns its own
    /// balance_channel driver, so a channel built under a dead runtime must
    /// never be reused. Test-only; production state is untouched.
    #[cfg(test)]
    pub(crate) fn reset_for_test() {
        SHARED_RPC.store(std::sync::Arc::new(None));
    }

    /// Get or lazily create a shared Channel for all unary RPC callers.
    /// Mirrors Go's `SharedRPC()` — returns the same singleton instance every time.
    /// Uses `balance_channel` so tonic load-balances across all configured endpoints,
    /// with auto-reconnect and HTTP/2 multiplexing on each connection.
    pub async fn get(api_config: &ApiConfig) -> anyhow::Result<Self> {
        let endpoints = api_config.effective_rpc_endpoints();

        // Fast path: reuse existing shared state.
        // Unlike the previous single-endpoint approach, the balance_channel
        // already includes all endpoints and tonic manages connection states internally.
        let guard = SHARED_RPC.load();
        if let Some(state) = guard.as_ref() {
            // Check if endpoints have changed — if so, dynamically update (mirrors Go's UpdateConfig).
            // This also runs for an empty membership so removed candidates are
            // purged from the channel before the caller fails closed below.
            if state.current_endpoints != endpoints {
                Self::apply_endpoint_changes_with_config(
                    api_config,
                    &state.endpoint_tx,
                    &state.current_endpoints,
                    &endpoints,
                )?;
                // Update the stored endpoint list under ArcSwap
                let mut new_state = (*state).clone();
                new_state.current_endpoints = endpoints.clone();
                SHARED_RPC.store(Arc::new(Some(new_state)));
            }
            if endpoints.is_empty() {
                anyhow::bail!("No RPC endpoints configured");
            }
            return Ok(Self {
                channel: state.channel.clone(),
                api_config: api_config.clone(),
            });
        }

        if endpoints.is_empty() {
            anyhow::bail!("No RPC endpoints configured");
        }

        // Slow path: build new balance_channel with all endpoints
        let (channel, endpoint_tx) = Channel::balance_channel::<String>(1024);
        Self::apply_endpoint_changes_with_config(api_config, &endpoint_tx, &[], &endpoints)?;

        SHARED_RPC.store(Arc::new(Some(SharedRpcState {
            channel: channel.clone(),
            endpoint_tx,
            current_endpoints: endpoints.clone(),
        })));

        Ok(Self {
            channel,
            api_config: api_config.clone(),
        })
    }

    /// Force-refresh the shared channel endpoints when API node changes.
    /// Mirrors Go's `UpdateConfig()` — replaces the endpoint list without
    /// rebuilding the Channel. Existing in-flight RPCs continue on their
    /// current connections; new RPCs route to the updated endpoints. An empty
    /// membership removes every endpoint instead of preserving a stale cache.
    pub fn refresh(api_config: &ApiConfig) -> anyhow::Result<()> {
        let endpoints = api_config.effective_rpc_endpoints();

        let guard = SHARED_RPC.load();
        if let Some(state) = guard.as_ref() {
            Self::apply_endpoint_changes_with_config(
                api_config,
                &state.endpoint_tx,
                &state.current_endpoints,
                &endpoints,
            )?;
            let mut new_state = (*state).clone();
            new_state.current_endpoints = endpoints.clone();
            SHARED_RPC.store(Arc::new(Some(new_state)));
            Ok(())
        } else {
            // No existing state — the next get() call will build it
            Ok(())
        }
    }

    /// Dynamically insert/remove endpoints on the balance_channel.
    /// This mirrors Go's `init()` which replaces the `conns` slice.
    #[cfg(test)]
    fn apply_endpoint_changes(
        endpoint_tx: &Sender<Change<String, Endpoint>>,
        old_endpoints: &[String],
        new_endpoints: &[String],
    ) -> anyhow::Result<()> {
        let api = ApiConfig {
            rpc_tls: Default::default(),
            rpc_endpoints: new_endpoints.to_vec(),
            rpc_disable_update: false,
            instance_id: "nd_0123456789".into(),
            secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
            comm_mode: String::new(),
            gcloud_node_id: "1".into(),
            access_log_pipeline: Default::default(),
            relay: Default::default(),
            kernel_tuning: Default::default(),
            cache: Default::default(),
        };
        Self::apply_endpoint_changes_with_config(&api, endpoint_tx, old_endpoints, new_endpoints)
    }

    fn apply_endpoint_changes_with_config(
        api: &ApiConfig,
        endpoint_tx: &Sender<Change<String, Endpoint>>,
        old_endpoints: &[String],
        new_endpoints: &[String],
    ) -> anyhow::Result<()> {
        // Build every new endpoint first: a URI or TLS error must not leave the
        // balance channel half-updated after old endpoints were already removed.
        let mut insertions = Vec::new();
        for new_ep in new_endpoints {
            if !old_endpoints.contains(new_ep) {
                let endpoint = RpcClient::configure_tls(Self::build_endpoint(new_ep)?, api)?;
                insertions.push((new_ep.clone(), endpoint));
            }
        }

        // Remove endpoints that are no longer in the list
        for old_ep in old_endpoints {
            if !new_endpoints.contains(old_ep) {
                endpoint_tx
                    .try_send(Change::Remove(old_ep.clone()))
                    .map_err(|err| {
                        anyhow::anyhow!("failed to remove RPC endpoint {}: {}", old_ep, err)
                    })?;
            }
        }
        // Insert new endpoints
        for (new_ep, endpoint) in insertions {
            endpoint_tx
                .try_send(Change::Insert(new_ep.clone(), endpoint))
                .map_err(|err| {
                    anyhow::anyhow!("failed to insert RPC endpoint {}: {}", new_ep, err)
                })?;
        }
        Ok(())
    }

    fn build_endpoint(uri: &str) -> anyhow::Result<Endpoint> {
        let uri = crate::api_config::validate_rpc_endpoint(uri)?;
        let endpoint = Channel::from_shared(uri.to_string())
            .map_err(|e| anyhow::anyhow!("Invalid URI {}: {}", uri, e))?;
        let endpoint = endpoint
            .user_agent("grpc-go/1.0")
            .map_err(|e| anyhow::anyhow!("Invalid user-agent for {}: {}", uri, e))?;
        let endpoint = RpcClient::configure_endpoint(endpoint, RpcConnectionProfile::Default);
        Ok(endpoint)
    }

    /// Convert to an RpcClient so all existing service factory methods work unchanged.
    /// This is zero-cost — Channel::clone just increments an Arc refcount.
    pub fn as_rpc_client(&self) -> RpcClient {
        RpcClient {
            channel: self.channel.clone(),
            api_config: self.api_config.clone(),
        }
    }
}

impl Clone for SharedRpcState {
    fn clone(&self) -> Self {
        Self {
            channel: self.channel.clone(),
            endpoint_tx: self.endpoint_tx.clone(),
            current_endpoints: self.current_endpoints.clone(),
        }
    }
}

/// Check if a tonic error is a connection error (mirrors Go's `IsConnError`).
/// Returns true for Unavailable or Canceled status codes, which indicate
/// transient network issues rather than application-level errors.
pub fn is_conn_error(status: &tonic::Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Cancelled
    ) || status.message().contains("code = Canceled")
}

#[cfg(test)]
mod tests {
    use super::{
        RPC_CONNECT_TIMEOUT, RPC_HTTP2_KEEPALIVE_INTERVAL, RPC_PING_CONNECT_TIMEOUT,
        RpcConnectionProfile, SharedRpcClient,
    };
    use tonic::transport::channel::Change;

    #[test]
    fn rpc_connection_profiles_match_expected_keepalive_policy() {
        assert_eq!(
            RpcConnectionProfile::Default.http2_keep_alive_interval(),
            Some(RPC_HTTP2_KEEPALIVE_INTERVAL)
        );
        assert_eq!(
            RpcConnectionProfile::Default.connect_timeout(),
            RPC_CONNECT_TIMEOUT
        );
        assert!(!RpcConnectionProfile::Default.keep_alive_while_idle());

        assert_eq!(
            RpcConnectionProfile::NodeStream.http2_keep_alive_interval(),
            None
        );
        assert_eq!(
            RpcConnectionProfile::NodeStream.connect_timeout(),
            RPC_CONNECT_TIMEOUT
        );

        assert_eq!(
            RpcConnectionProfile::PingProbe.http2_keep_alive_interval(),
            None
        );
        assert_eq!(
            RpcConnectionProfile::PingProbe.connect_timeout(),
            RPC_PING_CONNECT_TIMEOUT
        );
    }

    #[test]
    fn shared_rpc_endpoint_update_reports_channel_backpressure() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(Change::Remove("filled".to_string())).unwrap();

        let err =
            SharedRpcClient::apply_endpoint_changes(&tx, &[], &["http://127.0.0.1:1".to_string()])
                .unwrap_err();

        assert!(err.to_string().contains("failed to insert RPC endpoint"));
    }

    #[test]
    fn clearing_endpoint_list_removes_every_old_candidate() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        SharedRpcClient::apply_endpoint_changes(
            &tx,
            &[
                "http://127.0.0.1:1".to_string(),
                "http://127.0.0.1:2".to_string(),
            ],
            &[],
        )
        .unwrap();

        let mut removed = Vec::new();
        while let Ok(change) = rx.try_recv() {
            let Change::Remove(endpoint) = change else {
                panic!("unexpected endpoint change")
            };
            removed.push(endpoint);
        }
        assert_eq!(
            removed,
            vec![
                "http://127.0.0.1:1".to_string(),
                "http://127.0.0.1:2".to_string(),
            ]
        );
    }

    #[test]
    fn invalid_new_endpoint_does_not_remove_old_candidates() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let err = SharedRpcClient::apply_endpoint_changes(
            &tx,
            &["http://127.0.0.1:1".to_string()],
            &["http://127.0.0.1:2".to_string(), "not a url".to_string()],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("invalid RPC endpoint"),
            "unexpected error: {err}"
        );
        assert!(
            rx.try_recv().is_err(),
            "no removal may be sent before every new endpoint validates"
        );
    }

    fn test_api_config(endpoints: Vec<String>) -> crate::api_config::ApiConfig {
        crate::api_config::ApiConfig {
            rpc_tls: Default::default(),
            rpc_endpoints: endpoints,
            rpc_disable_update: false,
            instance_id: "nd_0123456789".into(),
            secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
            comm_mode: String::new(),
            gcloud_node_id: "1".into(),
            access_log_pipeline: Default::default(),
            relay: Default::default(),
            kernel_tuning: Default::default(),
            cache: Default::default(),
        }
    }

    /// Serializes tests that touch the process-global shared channel and the
    /// runtime membership, restoring both even when an assertion panics.
    struct SharedRpcTestGuard {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        runtime_endpoints: Option<Vec<String>>,
    }

    impl SharedRpcTestGuard {
        async fn new() -> Self {
            let lock = crate::api_config::RELEASE_SIGNING_TEST_LOCK.lock().await;
            let runtime_endpoints = crate::api_config::snapshot_runtime_rpc_endpoints();
            SharedRpcClient::reset_for_test();
            Self {
                _lock: lock,
                runtime_endpoints,
            }
        }
    }

    impl Drop for SharedRpcTestGuard {
        fn drop(&mut self) {
            SharedRpcClient::reset_for_test();
            crate::api_config::restore_runtime_rpc_endpoints(self.runtime_endpoints.clone());
        }
    }

    /// Negative control: the previous `refresh` rejected an empty membership
    /// before touching the balance channel, leaving removed endpoints cached in
    /// the shared state. `get` then still reported the stale list.
    #[tokio::test]
    async fn empty_membership_purges_shared_channel_and_fails_closed() {
        let _guard = SharedRpcTestGuard::new().await;
        let api = test_api_config(vec!["http://127.0.0.1:1".to_string()]);
        crate::api_config::ApiConfig::set_runtime_rpc_endpoints(api.rpc_endpoints.clone()).unwrap();
        SharedRpcClient::get(&api)
            .await
            .expect("the shared balance channel should build without connecting");

        crate::api_config::ApiConfig::set_runtime_rpc_endpoints(Vec::new()).unwrap();
        SharedRpcClient::refresh(&api).expect("empty membership refresh must not bail");

        {
            let guard = super::SHARED_RPC.load();
            let state = guard.as_ref().as_ref().expect("shared state");
            assert!(state.current_endpoints.is_empty());
        }
        assert!(SharedRpcClient::get(&api).await.is_err());
    }
}
