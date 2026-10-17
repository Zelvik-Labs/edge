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
}

/// Build the durable candidate membership from the control-plane aggregate.
///
/// `isOn` is the configured lifecycle state; heartbeat/ready state is
/// deliberately not consulted. A transiently offline API node therefore stays
/// in the membership, while disabling a node or deleting its address removes
/// it. Invalid addresses are reported separately so an all-invalid response
/// cannot be mistaken for an authoritative empty membership.
fn discover_api_endpoints(nodes: &[pb::ApiNode]) -> ApiEndpointDiscovery {
    let mut endpoints = Vec::new();
    let mut advertised = 0usize;
    let mut invalid = Vec::new();
    for node in nodes {
        if !node.is_on {
            continue;
        }
        for addr in &node.access_addrs {
            advertised += 1;
            match crate::api_config::validate_rpc_endpoint(addr) {
                Ok(endpoint) => {
                    if !endpoints.contains(&endpoint) {
                        endpoints.push(endpoint);
                    }
                }
                Err(err) => {
                    warn!(
                        "Ignoring invalid discovered API endpoint {:?}: {}",
                        addr, err
                    );
                    invalid.push(addr.clone());
                }
            }
        }
    }
    ApiEndpointDiscovery {
        endpoints,
        advertised,
        invalid,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ApiMembershipUpdate {
    Unchanged,
    Replace(Vec<String>),
    Clear,
    RejectInvalid,
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
            // is malformed input, not an authoritative empty membership. Keep
            // the working set instead of wiping it on a typo.
            return ApiMembershipUpdate::RejectInvalid;
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
        ApiMembershipUpdate::RejectInvalid => {
            report_node_log_with_context(
                api_config,
                "error",
                "API_NODE",
                "rejected discovered API node endpoints: every advertised address is invalid",
                None,
                Some("apiNodeEndpointRejected"),
                None,
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

            // Persist the full configured membership before switching runtime,
            // so a failed write leaves the previous (still valid) candidates
            // active and the next sync retries.
            let mut new_config = api_config.clone();
            new_config.rpc_endpoints = endpoints.clone();
            if let Err(e) = new_config.write_default() {
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

pub async fn sync_api_nodes(api_config: &ApiConfig) -> bool {
    if api_config.rpc_disable_update {
        return true;
    }
    let client = match SharedRpcClient::get(api_config).await {
        Ok(shared) => shared.as_rpc_client(),
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

    let discovery = discover_api_endpoints(&resp.api_nodes);
    if !discovery.invalid.is_empty() {
        warn!(
            "Ignored {} invalid access address(es) in the API node discovery response",
            discovery.invalid.len()
        );
    }

    let current = api_config.effective_rpc_endpoints();
    let update = plan_api_membership_update(&discovery, &current);
    apply_api_membership_update(api_config, update).await
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let discovery = discover_api_endpoints(&[
            api_node(
                1,
                true,
                &["https://api-a.example.com", "https://api-b.example.com"],
            ),
            api_node(2, true, &["https://api-c.example.com"]),
        ]);

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

        let discovery = discover_api_endpoints(&[node]);

        assert_eq!(
            discovery.endpoints,
            vec!["https://api-a.example.com".to_string()]
        );
    }

    #[test]
    fn disabled_nodes_remove_their_addresses_from_membership() {
        let discovery =
            discover_api_endpoints(&[api_node(1, false, &["https://api-a.example.com"])]);

        assert!(discovery.endpoints.is_empty());
        assert_eq!(discovery.advertised, 0);
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://api-a.example.com".to_string()]),
            ApiMembershipUpdate::Clear
        );
    }

    #[test]
    fn all_invalid_addresses_do_not_clear_working_membership() {
        let discovery = discover_api_endpoints(&[api_node(1, true, &["not a url"])]);

        assert!(discovery.endpoints.is_empty());
        assert_eq!(discovery.advertised, 1);
        assert_eq!(discovery.invalid, vec!["not a url".to_string()]);
        assert_eq!(
            plan_api_membership_update(&discovery, &["https://old.example.com".to_string()]),
            ApiMembershipUpdate::RejectInvalid
        );
    }

    #[test]
    fn duplicate_addresses_are_deduplicated_in_stable_first_seen_order() {
        let discovery = discover_api_endpoints(&[
            api_node(
                2,
                true,
                &["https://api-b.example.com", "https://api-a.example.com"],
            ),
            api_node(1, true, &["https://api-a.example.com"]),
        ]);

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
        let discovery =
            discover_api_endpoints(&[api_node(1, true, &["https://api-a.example.com"])]);

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
        };
        let update = plan_api_membership_update(&discovery, &api.effective_rpc_endpoints());
        assert_eq!(update, ApiMembershipUpdate::Replace(candidates.clone()));
        assert!(apply_api_membership_update(&api, update).await);

        let path = guard.home.join("configs").join("api_node.yaml");
        let persisted: ApiConfig =
            serde_yaml::from_slice(&std::fs::read(path).expect("persisted config"))
                .expect("persisted config parses");
        assert_eq!(persisted.rpc_endpoints, candidates);
        assert_eq!(api.effective_rpc_endpoints(), candidates);
    }
}
