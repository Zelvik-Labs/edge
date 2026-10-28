use crate::api_config::ApiConfig;
use crate::config::ConfigStore;
use crate::health_manager::GlobalHealthManager;
use crate::pb;
use crate::rpc::client::{RpcClient, SharedRpcClient};
use crate::rpc::logs::report_node_log_with_context;
use crate::rpc::utils::build_runtime_maps;
use crate::ssl::DynamicCertSelector;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

const API_NODE_SYNC_INTERVAL: Duration = Duration::from_secs(300);
const API_NODE_SYNC_MAX_INTERVAL: Duration = Duration::from_secs(900);
const UPDATING_SERVER_SYNC_BASE_INTERVAL: Duration = Duration::from_secs(60);
const UPDATING_SERVER_SYNC_MAX_INTERVAL: Duration = Duration::from_secs(900);

pub async fn start_api_node_syncer(api_config: ApiConfig) {
    if api_config.rpc_disable_update {
        return;
    }

    let mut delay = API_NODE_SYNC_INTERVAL;
    loop {
        let success = sync_api_nodes(&api_config).await;
        if success {
            delay = API_NODE_SYNC_INTERVAL;
        } else {
            delay = (delay.saturating_mul(2)).min(API_NODE_SYNC_MAX_INTERVAL);
        }
        tokio::time::sleep(delay).await;
    }
}

pub async fn start_updating_server_list_syncer(
    api_config: ApiConfig,
    config_store: Arc<ConfigStore>,
    health_manager: Arc<GlobalHealthManager>,
    cert_selector: Arc<DynamicCertSelector>,
) {
    let mut delay = UPDATING_SERVER_SYNC_BASE_INTERVAL;
    let mut last_id = 0i64;

    loop {
        if config_store.get_node_id().await <= 0 {
            tokio::time::sleep(UPDATING_SERVER_SYNC_BASE_INTERVAL).await;
            continue;
        }
        let baseline = config_store.get_updating_server_list_id().await;
        if last_id <= 0 || baseline > last_id {
            last_id = baseline;
        }
        let success = sync_updating_server_list_once(
            &api_config,
            config_store.as_ref(),
            health_manager.as_ref(),
            cert_selector.as_ref(),
            &mut last_id,
        )
        .await;
        if success {
            delay = UPDATING_SERVER_SYNC_BASE_INTERVAL;
        } else {
            delay = (delay.saturating_mul(2)).min(UPDATING_SERVER_SYNC_MAX_INTERVAL);
        }
        tokio::time::sleep(delay).await;
    }
}

pub async fn sync_updating_server_list_once(
    api_config: &ApiConfig,
    config_store: &ConfigStore,
    health_manager: &GlobalHealthManager,
    cert_selector: &DynamicCertSelector,
    last_id: &mut i64,
) -> bool {
    if *last_id <= 0 {
        *last_id = config_store.get_updating_server_list_id().await;
    }

    let client = match SharedRpcClient::get(api_config).await {
        Ok(shared) => shared.as_rpc_client(),
        Err(e) => {
            warn!("Failed to connect for updating server sync: {}", e);
            report_node_log_with_context(
                api_config,
                "warn",
                "API_NODE",
                &format!("failed to connect for updating server sync: {}", e),
                None,
                Some("updatingServerSyncConnectFailed"),
                None,
            )
            .await;
            return false;
        }
    };

    let mut service = client.updating_server_list_service();
    match crate::rpc::track_rpc(
        service
            .find_updating_server_lists(pb::FindUpdatingServerListsRequest { last_id: *last_id }),
    )
    .await
    {
        Ok(resp) => {
            let resp = resp.into_inner();

            if !resp.servers_json.is_empty() {
                match serde_json::from_slice::<Vec<crate::config_models::ServerConfig>>(
                    &resp.servers_json,
                ) {
                    Ok(servers) => {
                        let changed = servers
                            .iter()
                            .filter_map(|server| server.id)
                            .collect::<std::collections::HashSet<_>>();
                        let (node_level, parent_nodes, tiered_origin_bypass, allow_lan) =
                            config_store.get_origin_runtime_context().await;
                        let global_http =
                            Some((*config_store.get_global_http_config_sync()).clone());
                        let maps = build_runtime_maps(
                            servers,
                            health_manager,
                            node_level,
                            parent_nodes,
                            tiered_origin_bypass,
                            allow_lan,
                            global_http,
                            resp.servers_json.len() as u64,
                        )
                        .await;
                        let added = maps
                            .all_servers
                            .into_iter()
                            .filter(|server| server.is_on)
                            .collect::<Vec<_>>();
                        config_store
                            .replace_servers_snapshot(
                                changed,
                                added,
                                maps.servers,
                                maps.routes,
                                maps.id_to_lb,
                            )
                            .await;
                        refresh_certificates(config_store, cert_selector).await;
                    }
                    Err(e) => {
                        warn!("Failed to parse updating servers JSON: {}", e);
                        report_node_log_with_context(
                            api_config,
                            "error",
                            "API_NODE",
                            &format!("failed to parse updating servers JSON: {}", e),
                            None,
                            Some("updatingServerSyncDecodeFailed"),
                            None,
                        )
                        .await;
                        return false;
                    }
                }
            }
            *last_id = (*last_id).max(resp.max_id);
            true
        }
        Err(e) => {
            warn!("Failed to sync updating server list: {}", e);
            report_node_log_with_context(
                api_config,
                "warn",
                "API_NODE",
                &format!("failed to sync updating server list: {}", e),
                None,
                Some("updatingServerSyncFailed"),
                None,
            )
            .await;
            false
        }
    }
}

async fn refresh_certificates(config_store: &ConfigStore, cert_selector: &DynamicCertSelector) {
    let certs = config_store.collect_ssl_config().await;
    crate::ssl::sync_certs_for_servers(cert_selector, &certs, &config_store.get_all_servers_sync())
        .await;
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ApiEndpointDiscovery {
    /// Configured members in stable first-seen order.
    endpoints: Vec<String>,
    /// Raw access addresses advertised by enabled API nodes, including invalid ones.
    advertised: usize,
    /// Advertised addresses rejected by endpoint validation.
    invalid: Vec<String>,
    /// Advertised addresses whose scheme does not match this node's commMode.
    incompatible: Vec<String>,
}

/// Build the durable candidate membership from the control-plane aggregate.
///
/// `isOn` is the configured lifecycle state; heartbeat/ready state is
/// deliberately not consulted. A transiently offline API node therefore stays
/// in the membership, while disabling a node or deleting its address removes
/// it. Invalid addresses and addresses whose scheme does not match the node's
/// registered transport mode are reported separately so an all-invalid
/// response cannot be mistaken for an authoritative empty membership.
fn discover_api_endpoints(
    nodes: &[pb::ApiNode],
    comm_mode: zelvik_suite::CommMode,
) -> ApiEndpointDiscovery {
    let mut endpoints = Vec::new();
    let mut advertised = 0usize;
    let mut invalid = Vec::new();
    let mut incompatible = Vec::new();
    for node in nodes {
        if !node.is_on {
            continue;
        }
        for addr in &node.access_addrs {
            advertised += 1;
            let endpoint = match crate::api_config::validate_rpc_endpoint(addr) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    warn!(
                        "Ignoring invalid discovered API endpoint {:?}: {}",
                        addr, err
                    );
                    invalid.push(addr.clone());
                    continue;
                }
            };
            if !crate::api_config::rpc_endpoint_matches_comm_mode(&endpoint, comm_mode) {
                // Never persist a candidate that would force this node to speak
                // a different transport mode. This is the last local check;
                // the control-plane aggregate is filtered by commMode too.
                warn!(
                    "Ignoring discovered API endpoint {:?}: scheme does not match commMode {:?}",
                    addr,
                    comm_mode.as_str()
                );
                incompatible.push(addr.clone());
                continue;
            }
            if !endpoints.contains(&endpoint) {
                endpoints.push(endpoint);
            }
        }
    }
    ApiEndpointDiscovery {
        endpoints,
        advertised,
        invalid,
        incompatible,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ApiMembershipUpdate {
    Unchanged,
    Replace(Vec<String>),
    Clear,
    RejectInvalid {
        invalid: Vec<String>,
        incompatible: Vec<String>,
    },
}

fn plan_api_membership_update(
    discovery: &ApiEndpointDiscovery,
    current: &[String],
) -> ApiMembershipUpdate {
    if discovery.endpoints.as_slice() == current {
        return ApiMembershipUpdate::Unchanged;
    }
    if discovery.endpoints.is_empty() {
        if discovery.advertised > 0 {
            // A successful response whose advertised addresses are all invalid
            // or incompatible is malformed input for this node, not an
            // authoritative empty membership. Keep the working set instead of
            // wiping it on a typo or a transport-mode mismatch.
            return ApiMembershipUpdate::RejectInvalid {
                invalid: discovery.invalid.clone(),
                incompatible: discovery.incompatible.clone(),
            };
        }
        return ApiMembershipUpdate::Clear;
    }
    ApiMembershipUpdate::Replace(discovery.endpoints.clone())
}

/// Observe candidate connectivity for diagnostics only. The returned list is
/// never used to filter membership.
async fn observe_endpoint_connectivity(
    api_config: &ApiConfig,
    endpoints: &[String],
) -> Vec<String> {
    let mut healthy = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        match RpcClient::probe_endpoint(api_config, endpoint).await {
            Ok(()) => healthy.push(endpoint.clone()),
            Err(status) => {
                debug!("API endpoint {} failed health probe: {}", endpoint, status);
            }
        }
    }
    healthy
}

async fn apply_api_membership_update(api_config: &ApiConfig, update: ApiMembershipUpdate) -> bool {
    match update {
        ApiMembershipUpdate::Unchanged => true,
        ApiMembershipUpdate::RejectInvalid {
            invalid,
            incompatible,
        } => {
            report_node_log_with_context(
                api_config,
                "error",
                "API_NODE",
                "rejected discovered API node endpoints: every advertised address is invalid or incompatible with commMode",
                None,
                Some("apiNodeEndpointRejected"),
                Some(serde_json::json!({
                    "invalid": invalid,
                    "incompatible": incompatible,
                })),
            )
            .await;
            false
        }
        ApiMembershipUpdate::Clear => {
            // An authoritative empty membership clears the runtime candidates.
            // The local `rpc.endpoints` seed is intentionally left in place:
            // it is the only way a restarted node can reach the control plane
            // and discover membership again. It is not consulted while the
            // runtime override exists.
            if let Err(e) = ApiConfig::set_runtime_rpc_endpoints(Vec::new()) {
                warn!("Failed to clear discovered API node endpoints: {}", e);
                report_node_log_with_context(
                    api_config,
                    "error",
                    "API_NODE",
                    &format!("failed to clear discovered API node endpoints: {}", e),
                    None,
                    Some("apiNodeEndpointRejected"),
                    None,
                )
                .await;
                return false;
            }
            if let Err(e) = SharedRpcClient::refresh(api_config) {
                warn!(
                    "Failed to refresh shared RPC channel after clearing API node membership: {}",
                    e
                );
            }
            warn!("API node membership is empty; cleared runtime RPC candidates");
            true
        }
        ApiMembershipUpdate::Replace(endpoints) => {
            // Observe connectivity before switching, but persist the complete
            // configured set regardless of ping outcome.
            let healthy = observe_endpoint_connectivity(api_config, &endpoints).await;
            if healthy.len() != endpoints.len() {
                let unhealthy = endpoints
                    .iter()
                    .filter(|endpoint| !healthy.iter().any(|candidate| candidate == *endpoint))
                    .cloned()
                    .collect::<Vec<_>>();
                report_node_log_with_context(
                    api_config,
                    "warn",
                    "API_NODE",
                    &format!(
                        "discovered API node connectivity: {}/{} endpoints healthy",
                        healthy.len(),
                        endpoints.len()
                    ),
                    None,
                    Some("apiNodePingFailed"),
                    Some(serde_json::json!({
                        "healthy": healthy,
                        "unhealthy": unhealthy,
                    })),
                )
                .await;
            }

            // Persist only `rpc.endpoints` into the existing YAML mapping, so
            // operator-owned/unknown keys (for example `xdp`) survive. Write
            // before switching runtime so a failure leaves the previous
            // candidates active and the next sync retries.
            if let Err(e) = api_config.write_rpc_endpoints_default(&endpoints) {
                debug!("Failed to write updated api_node config: {}", e);
                report_node_log_with_context(
                    api_config,
                    "error",
                    "API_NODE",
                    &format!("failed to write updated api_node config: {}", e),
                    None,
                    Some("apiNodeConfigWriteFailed"),
                    Some(serde_json::json!({ "endpoints": endpoints })),
                )
                .await;
                return false;
            }
            if let Err(e) = ApiConfig::set_runtime_rpc_endpoints(endpoints.clone()) {
                warn!("Rejected discovered API node endpoints: {}", e);
                report_node_log_with_context(
                    api_config,
                    "error",
                    "API_NODE",
                    &format!("rejected discovered API node endpoints: {}", e),
                    None,
                    Some("apiNodeEndpointRejected"),
                    Some(serde_json::json!({ "endpoints": endpoints })),
                )
                .await;
                return false;
            }
            if let Err(e) = SharedRpcClient::refresh(api_config) {
                warn!(
                    "Failed to refresh shared RPC channel after endpoint change: {}",
                    e
                );
            }
            true
        }
    }
}

/// Build the client used only for the discovery RPC. When the data-plane
/// membership is empty or contains no endpoint compatible with this node's
/// commMode, use the local `rpc.endpoints` seed so a re-enabled API can be
/// observed without a process restart. Data-plane RPCs keep using
/// `SharedRpcClient`/`effective_rpc_endpoints`, which remains fail-closed.
async fn discovery_client(api_config: &ApiConfig) -> anyhow::Result<RpcClient> {
    let membership = api_config.effective_rpc_endpoints();
    let discovery = api_config.discovery_rpc_endpoints();
    if discovery.is_empty() {
        anyhow::bail!("no RPC endpoint is usable for API node discovery");
    }
    if discovery == membership {
        return Ok(SharedRpcClient::get(api_config).await?.as_rpc_client());
    }
    debug!(
        "using discovery-only RPC endpoints because the authoritative membership is empty or incompatible: {:?}",
        discovery
    );
    RpcClient::new_with_endpoints(api_config, &discovery, false).await
}

pub async fn sync_api_nodes(api_config: &ApiConfig) -> bool {
    if api_config.rpc_disable_update {
        return true;
    }
    let client = match discovery_client(api_config).await {
        Ok(client) => client,
        Err(e) => {
            debug!("Failed to connect for api-node sync: {}", e);
            report_node_log_with_context(
                api_config,
                "warn",
                "API_NODE",
                &format!("failed to connect for api-node sync: {}", e),
                None,
                Some("apiNodeSyncConnectFailed"),
                None,
            )
            .await;
            return false;
        }
    };
    let mut api_node_service = client.api_node_service();

    let resp = match crate::rpc::track_rpc(
        api_node_service.find_all_enabled_api_nodes(pb::FindAllEnabledApiNodesRequest {}),
    )
    .await
    {
        Ok(resp) => resp.into_inner(),
        // A request error leaves both the durable and runtime membership
        // untouched: only a successful aggregate is authoritative.
        Err(e) => {
            debug!("Failed to list api nodes: {}", e);
            report_node_log_with_context(
                api_config,
                "warn",
                "API_NODE",
                &format!("failed to list api nodes: {}", e),
                None,
                Some("apiNodeListFailed"),
                None,
            )
            .await;
            return false;
        }
    };

    let comm_mode = match api_config.comm_mode() {
        Ok(mode) => mode,
        Err(e) => {
            report_node_log_with_context(
                api_config,
                "error",
                "API_NODE",
                &format!(
                    "rejected discovered API node endpoints: invalid commMode: {}",
                    e
                ),
                None,
                Some("apiNodeEndpointRejected"),
                None,
            )
            .await;
            return false;
        }
    };

    let discovery = discover_api_endpoints(&resp.api_nodes, comm_mode);
    if !discovery.invalid.is_empty() {
        warn!(
            "Ignored {} invalid access address(es) in the API node discovery response",
            discovery.invalid.len()
        );
    }
    if !discovery.incompatible.is_empty() {
        warn!(
            "Ignored {} access address(es) incompatible with commMode {} in the API node discovery response",
            discovery.incompatible.len(),
            comm_mode.as_str()
        );
    }

    let current = api_config.effective_rpc_endpoints();
    let update = plan_api_membership_update(&discovery, &current);
    apply_api_membership_update(api_config, update).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use zelvik_suite::CommMode;

    fn api_node(id: i64, is_on: bool, access_addrs: &[&str]) -> pb::ApiNode {
        pb::ApiNode {
            id,
            is_on,
            access_addrs: access_addrs
                .iter()
                .map(|addr| (*addr).to_string())
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn discovery_keeps_every_configured_address_for_enabled_nodes() {
        let discovery = discover_api_endpoints(
            &[
                api_node(
                    1,
                    true,
                    &["https://api-a.example.com", "https://api-b.example.com"],
                ),
                api_node(2, true, &["https://api-c.example.com"]),
            ],
            CommMode::Tls,
        );

        assert_eq!(
            discovery.endpoints,
            vec![
                "https://api-a.example.com",
                "https://api-b.example.com",
                "https://api-c.example.com",
            ]
        );
        assert_eq!(discovery.advertised, 3);
        assert!(discovery.invalid.is_empty());
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://old.example.com".to_string()]),
            ApiMembershipUpdate::Replace(discovery.endpoints.clone())
        );
    }

    #[test]
    fn discovery_uses_configured_lifecycle_not_heartbeat_state() {
        let mut node = api_node(1, true, &["https://api-a.example.com"]);
        node.status_json = br#"{"isOn":false,"heartbeat":{"healthy":false}}"#.to_vec();

        let discovery = discover_api_endpoints(&[node], CommMode::Tls);

        assert_eq!(
            discovery.endpoints,
            vec!["https://api-a.example.com".to_string()]
        );
    }

    #[test]
    fn disabled_nodes_remove_their_addresses_from_membership() {
        let discovery = discover_api_endpoints(
            &[api_node(1, false, &["https://api-a.example.com"])],
            CommMode::Tls,
        );

        assert!(discovery.endpoints.is_empty());
        assert_eq!(discovery.advertised, 0);
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://api-a.example.com".to_string()]),
            ApiMembershipUpdate::Clear
        );
    }

    #[test]
    fn all_invalid_addresses_do_not_clear_working_membership() {
        let discovery = discover_api_endpoints(&[api_node(1, true, &["not a url"])], CommMode::Tls);

        assert!(discovery.endpoints.is_empty());
        assert_eq!(discovery.advertised, 1);
        assert_eq!(discovery.invalid, vec!["not a url".to_string()]);
        assert!(discovery.incompatible.is_empty());
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://old.example.com".to_string()]),
            ApiMembershipUpdate::RejectInvalid {
                invalid: vec!["not a url".to_string()],
                incompatible: Vec::new(),
            }
        );
    }

    #[test]
    fn discovery_rejects_plaintext_endpoint_for_tls_node() {
        let discovery = discover_api_endpoints(
            &[api_node(1, true, &["http://api-a.example.com"])],
            CommMode::Tls,
        );

        assert!(discovery.endpoints.is_empty());
        assert_eq!(discovery.advertised, 1);
        assert!(discovery.invalid.is_empty());
        assert_eq!(
            discovery.incompatible,
            vec!["http://api-a.example.com".to_string()]
        );
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://old.example.com".to_string()]),
            ApiMembershipUpdate::RejectInvalid {
                invalid: Vec::new(),
                incompatible: vec!["http://api-a.example.com".to_string()],
            }
        );
    }

    #[test]
    fn discovery_rejects_https_endpoint_for_plaintext_node() {
        let discovery = discover_api_endpoints(
            &[api_node(1, true, &["https://api-a.example.com"])],
            CommMode::Plaintext,
        );

        assert!(discovery.endpoints.is_empty());
        assert!(discovery.invalid.is_empty());
        assert_eq!(
            discovery.incompatible,
            vec!["https://api-a.example.com".to_string()]
        );
    }

    #[test]
    fn discovery_persists_only_scheme_compatible_members() {
        let discovery = discover_api_endpoints(
            &[api_node(
                1,
                true,
                &["http://api-a.example.com", "https://api-b.example.com"],
            )],
            CommMode::Tls,
        );

        assert_eq!(
            discovery.endpoints,
            vec!["https://api-b.example.com".to_string()]
        );
        assert_eq!(
            discovery.incompatible,
            vec!["http://api-a.example.com".to_string()]
        );
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://old.example.com".to_string()]),
            ApiMembershipUpdate::Replace(vec!["https://api-b.example.com".to_string()])
        );
    }

    #[test]
    fn duplicate_addresses_are_deduplicated_in_stable_first_seen_order() {
        let discovery = discover_api_endpoints(
            &[
                api_node(
                    2,
                    true,
                    &["https://api-b.example.com", "https://api-a.example.com"],
                ),
                api_node(1, true, &["https://api-a.example.com"]),
            ],
            CommMode::Tls,
        );

        assert_eq!(
            discovery.endpoints,
            vec![
                "https://api-b.example.com".to_string(),
                "https://api-a.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn unchanged_membership_is_not_rewritten() {
        let discovery = discover_api_endpoints(
            &[api_node(1, true, &["https://api-a.example.com"])],
            CommMode::Tls,
        );

        assert_eq!(
            plan_api_membership_update(&discovery, &["https://api-a.example.com".to_string()]),
            ApiMembershipUpdate::Unchanged
        );
    }

    fn test_api_config(endpoints: Vec<String>) -> ApiConfig {
        ApiConfig {
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

    /// Serializes the process-global runtime membership/env and restores both.
    struct TestHomeGuard {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        saved_home: Option<std::ffi::OsString>,
        saved_runtime: Option<Vec<String>>,
        home: std::path::PathBuf,
    }

    impl TestHomeGuard {
        async fn new() -> Self {
            let lock = crate::api_config::RELEASE_SIGNING_TEST_LOCK.lock().await;
            let saved_home = std::env::var_os("ZELVIK_EDGE_HOME");
            let saved_runtime = crate::api_config::snapshot_runtime_rpc_endpoints();
            let home = std::env::temp_dir().join(format!(
                "zelvik-api-node-membership-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(home.join("configs")).unwrap();
            unsafe {
                std::env::set_var("ZELVIK_EDGE_HOME", &home);
            }
            SharedRpcClient::reset_for_test();
            Self {
                _lock: lock,
                saved_home,
                saved_runtime,
                home,
            }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            SharedRpcClient::reset_for_test();
            crate::api_config::restore_runtime_rpc_endpoints(self.saved_runtime.clone());
            match &self.saved_home {
                Some(value) => unsafe { std::env::set_var("ZELVIK_EDGE_HOME", value) },
                None => unsafe { std::env::remove_var("ZELVIK_EDGE_HOME") },
            }
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    /// Negative control: the previous update path persisted only endpoints
    /// that passed the ping probe, so a temporary outage (or a probe from a
    /// node with no route yet) replaced the configured list with a partial
    /// one. The full authoritative membership must be written even when every
    /// probe fails.
    #[tokio::test]
    async fn full_membership_is_persisted_when_no_probe_succeeds() {
        let guard = TestHomeGuard::new().await;
        let candidates = vec![
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:2".to_string(),
        ];
        let api = test_api_config(vec!["http://127.0.0.1:9".to_string()]);
        ApiConfig::set_runtime_rpc_endpoints(api.rpc_endpoints.clone()).unwrap();

        let discovery = ApiEndpointDiscovery {
            endpoints: candidates.clone(),
            advertised: candidates.len(),
            invalid: Vec::new(),
            incompatible: Vec::new(),
        };
        let update = plan_api_membership_update(&discovery, &api.effective_rpc_endpoints());
        assert_eq!(update, ApiMembershipUpdate::Replace(candidates.clone()));
        assert!(apply_api_membership_update(&api, update).await);

        let path = guard.home.join("data").join("rpc-endpoints.json");
        let persisted: Vec<String> =
            serde_json::from_slice(&std::fs::read(path).expect("persisted membership"))
                .expect("persisted membership parses");
        assert_eq!(persisted, candidates);
        assert_eq!(api.effective_rpc_endpoints(), candidates);
    }

    /// Finding 3: the membership write must merge into the existing YAML
    /// mapping, not serialize the typed struct over it. Parse the result as
    /// raw YAML so a dropped operator-owned key is visible.
    #[tokio::test]
    async fn membership_write_preserves_operator_owned_yaml_keys() {
        let guard = TestHomeGuard::new().await;
        let path = guard.home.join("configs").join("api_node.yaml");
        std::fs::write(
            &path,
            r#"rpc.endpoints:
  - "http://127.0.0.1:9"
instanceId: "nd_0123456789"
secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
commMode: "plaintext"
xdp:
  enabled: false
operatorUnknown:
  keep: "yes"
kernelTuning:
  enabled: false
"#,
        )
        .unwrap();
        let api: ApiConfig =
            serde_yaml::from_slice(&std::fs::read(&path).expect("seed config")).unwrap();
        let candidates = vec![
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:2".to_string(),
        ];

        api.write_rpc_endpoints_default(&candidates).unwrap();

        let raw: serde_yaml::Value =
            serde_yaml::from_slice(&std::fs::read(&path).expect("written config")).unwrap();
        assert_eq!(
            raw.get("xdp")
                .and_then(|value| value.get("enabled"))
                .and_then(serde_yaml::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            raw.get("operatorUnknown")
                .and_then(|value| value.get("keep"))
                .and_then(serde_yaml::Value::as_str),
            Some("yes")
        );
        assert_eq!(
            raw.get("kernelTuning")
                .and_then(|value| value.get("enabled"))
                .and_then(serde_yaml::Value::as_bool),
            Some(false)
        );
        let persisted = raw
            .get("rpc.endpoints")
            .and_then(serde_yaml::Value::as_sequence)
            .expect("rpc.endpoints sequence")
            .iter()
            .map(|value| value.as_str().expect("endpoint string").to_string())
            .collect::<Vec<_>>();
        assert_eq!(persisted, vec!["http://127.0.0.1:9".to_string()]);
        let membership: Vec<String> = serde_json::from_slice(
            &std::fs::read(guard.home.join("data/rpc-endpoints.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(membership, candidates);
    }

    /// Finding 1: an authoritative empty membership must not remove the
    /// ability to ask the control plane again. The discovery client must use
    /// the local seed while the data-plane membership is empty, and a
    /// non-empty aggregate must restore the membership without a restart.
    #[tokio::test]
    async fn empty_membership_keeps_discovery_reachable_and_restores_candidates() {
        let _guard = TestHomeGuard::new().await;
        let api = test_api_config(vec!["http://127.0.0.1:1".to_string()]);
        api.write_default().unwrap();
        ApiConfig::set_runtime_rpc_endpoints(Vec::new()).unwrap();
        assert!(api.effective_rpc_endpoints().is_empty());

        let error = discovery_client(&api)
            .await
            .err()
            .expect("the seed endpoint is unreachable, so the attempt must fail");
        let message = error.to_string();
        assert!(
            message.contains("http://127.0.0.1:1"),
            "discovery did not use the local seed: {message}"
        );
        assert!(
            !message.contains("No RPC endpoints configured"),
            "discovery used the empty data-plane membership: {message}"
        );

        let discovery = ApiEndpointDiscovery {
            endpoints: vec!["http://127.0.0.1:2".to_string()],
            advertised: 1,
            invalid: Vec::new(),
            incompatible: Vec::new(),
        };
        let update = plan_api_membership_update(&discovery, &api.effective_rpc_endpoints());
        assert_eq!(
            update,
            ApiMembershipUpdate::Replace(vec!["http://127.0.0.1:2".to_string()])
        );
        assert!(apply_api_membership_update(&api, update).await);
        assert_eq!(
            api.effective_rpc_endpoints(),
            vec!["http://127.0.0.1:2".to_string()]
        );
    }
}
