//! Apply NodeControl payloads through `ConfigStore::update_config`.
//!
//! Parse/build happens before the write; failure leaves the previous active
//! snapshot. Kernel XDP / WAF attach side effects stay on the legacy RPC path.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::ConfigStore;
use crate::config_models::{
    GRPCConfig, GlobalHTTPAllConfig, HTTP3Policy, HTTPCCPolicy, HTTPCachePolicy, HTTPPagesPolicy,
    NodeConfigPayload, ParentNodeConfig, ServerConfig, UAMPolicy, WebPImagePolicy,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreApplyResult {
    pub node_id: i64,
    pub config_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreApplyError {
    InvalidPayload(String),
    MetricCapacity { used: usize, limit: usize },
}

fn parse_i64_map<T: Clone>(raw: &HashMap<String, T>) -> HashMap<i64, T> {
    raw.iter()
        .filter_map(|(key, value)| key.parse::<i64>().ok().map(|id| (id, value.clone())))
        .collect()
}

type ServerIndex = (
    Vec<Arc<ServerConfig>>,
    HashMap<String, Arc<ServerConfig>>,
    HashMap<String, Arc<crate::lb_factory::AnyLoadBalancer>>,
    HashMap<i64, Arc<crate::lb_factory::AnyLoadBalancer>>,
);

fn index_servers(servers: &[ServerConfig]) -> ServerIndex {
    let mut all_servers = Vec::new();
    let mut by_name = HashMap::new();
    let mut routes = HashMap::new();
    let mut id_to_lb = HashMap::new();
    let (fallback_lb, _) = crate::rpc::utils::fallback_runtime_lb();

    for server in servers {
        if !server.is_on {
            continue;
        }
        let arc = Arc::new(server.clone());
        all_servers.push(arc.clone());
        let names = crate::rpc::utils::server_runtime_names(server);
        let server_id = server.numeric_id();
        if server_id > 0 {
            id_to_lb.insert(server_id, fallback_lb.clone());
        }
        if names.is_empty() {
            let key = format!("__id_{}", server_id);
            by_name.insert(key.clone(), arc);
            routes.insert(key, fallback_lb.clone());
        } else {
            for name in names {
                by_name.insert(name.clone(), arc.clone());
                routes.insert(name, fallback_lb.clone());
            }
        }
    }
    (all_servers, by_name, routes, id_to_lb)
}

pub async fn apply_node_payload_json(
    store: &ConfigStore,
    json: &[u8],
) -> Result<StoreApplyResult, StoreApplyError> {
    let payload: NodeConfigPayload = serde_json::from_slice(json)
        .map_err(|err| StoreApplyError::InvalidPayload(err.to_string()))?;
    apply_node_payload(store, payload).await
}

pub async fn apply_node_payload(
    store: &ConfigStore,
    payload: NodeConfigPayload,
) -> Result<StoreApplyResult, StoreApplyError> {
    let node_id = payload.id.unwrap_or(0);
    let config_version = payload.version.unwrap_or(0);
    // Reserve metric instances before building host maps: index_servers only
    // keeps enabled sites, and the clones below carry the binding into
    // `update_config`, so the install cannot fail on capacity afterwards.
    if let Err(err) = crate::metrics::METRICS
        .admit_configs(payload.servers.iter().filter(|server| server.is_on))
    {
        return Err(StoreApplyError::MetricCapacity {
            used: err.used,
            limit: err.limit,
        });
    }
    let (all_servers, servers, routes, id_to_lb) = index_servers(&payload.servers);
    let deleted_contents = store.get_deleted_contents().await;

    let mut allow_lan_ip = false;
    let mut force_ln_request = false;
    let mut ln_method = "random".to_string();
    let mut supports_low_version_http = false;
    let mut match_cert_from_all_servers = false;
    let mut server_name = String::new();
    let mut enable_server_addr_variable = false;
    let mut request_origins_with_encodings = false;
    let mut xff_max_addresses = 0;
    let mut match_domain_strictly = false;
    let mut node_ip_show_page = false;
    let mut node_ip_page_html = String::new();
    let mut domain_mismatch_action = None;
    let mut global_http: Option<GlobalHTTPAllConfig> = None;

    if let Some(gsc) = &payload.global_server_config
        && let Some(http_all) = &gsc.http_all
    {
        allow_lan_ip = http_all.allow_lan_ip;
        force_ln_request = http_all.force_ln_request;
        ln_method = http_all.ln_request_scheduling_method.clone();
        supports_low_version_http = http_all.supports_low_version_http;
        match_cert_from_all_servers = http_all.match_cert_from_all_servers;
        server_name = http_all.server_name.clone();
        enable_server_addr_variable = http_all.enable_server_addr_variable;
        request_origins_with_encodings = http_all.request_origins_with_encodings;
        xff_max_addresses = http_all.xff_max_addresses;
        match_domain_strictly = http_all.match_domain_strictly;
        node_ip_show_page = http_all.node_ip_show_page;
        node_ip_page_html = http_all.node_ip_page_html.clone();
        domain_mismatch_action = http_all.domain_mismatch_action.clone();
        global_http = Some(http_all.clone());
    }

    let node_cluster_id = payload.node_cluster.as_ref().map(|c| c.id).unwrap_or(0);
    let grpc_policy: Option<GRPCConfig> = payload
        .grpc_policies
        .get(&node_cluster_id.to_string())
        .cloned()
        .or_else(|| payload.grpc_policies.get("0").cloned())
        .or_else(|| payload.grpc_policies.values().next().cloned())
        .or_else(|| payload.primary_grpc_policy.clone());

    let parent_nodes: HashMap<i64, Vec<ParentNodeConfig>> = parse_i64_map(&payload.parent_nodes);
    let uam_policies: HashMap<i64, UAMPolicy> = parse_i64_map(&payload.uam_policies);
    let http_cc_policies: HashMap<i64, HTTPCCPolicy> = parse_i64_map(&payload.http_cc_policies);
    let http3_policies: HashMap<i64, HTTP3Policy> = parse_i64_map(&payload.http3_policies);
    let http_pages_policies: HashMap<i64, HTTPPagesPolicy> =
        parse_i64_map(&payload.http_pages_policies);
    let webp_image_policies: HashMap<i64, WebPImagePolicy> =
        parse_i64_map(&payload.webp_image_policies);
    let cache_policy: Vec<Arc<HTTPCachePolicy>> = payload
        .http_cache_policies
        .iter()
        .map(|policy| Arc::new(policy.clone()))
        .collect();

    store
        .update_config(
            node_id,
            config_version,
            payload.node_region.as_ref().map(|r| r.id).unwrap_or(0),
            node_cluster_id,
            all_servers,
            servers,
            routes,
            id_to_lb,
            deleted_contents,
            payload.global_pages.clone(),
            payload.metric_items.clone(),
            payload.ssl_certs.clone(),
            payload.ssl_policy.clone(),
            payload.updating_server_list_id,
            payload.level,
            payload.is_on,
            payload.enable_ip_lists,
            parent_nodes,
            false,
            force_ln_request,
            ln_method,
            HashMap::new(),
            grpc_policy,
            supports_low_version_http,
            match_cert_from_all_servers,
            server_name,
            enable_server_addr_variable,
            request_origins_with_encodings,
            xff_max_addresses,
            allow_lan_ip,
            match_domain_strictly,
            node_ip_show_page,
            node_ip_page_html,
            domain_mismatch_action,
            global_http,
            cache_policy,
            payload.http_firewall_policies.clone(),
            payload.waf_actions.clone(),
            uam_policies,
            http_cc_policies,
            http3_policies,
            http_pages_policies,
            webp_image_policies,
            payload.toa.clone(),
            payload
                .global_server_config
                .as_ref()
                .and_then(|g| g.http_access_log.clone()),
        )
        .await
        .map_err(|err| StoreApplyError::MetricCapacity {
            used: err.used,
            limit: err.limit,
        })?;

    Ok(StoreApplyResult {
        node_id,
        config_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_payload(version: i64, host: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": 42,
            "version": version,
            "isOn": true,
            "servers": [{
                "id": 7,
                "isOn": true,
                "serverNames": [{"name": host}]
            }]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn successful_apply_replaces_active_snapshot() {
        let store = ConfigStore::new();
        apply_node_payload_json(&store, &sample_payload(11, "a.example.test"))
            .await
            .unwrap();
        assert_eq!(store.get_config_version().await, 11);
        assert!(store.get_server_sync("a.example.test").is_some());
    }

    #[tokio::test]
    async fn invalid_json_keeps_previous_active() {
        let store = ConfigStore::new();
        apply_node_payload_json(&store, &sample_payload(3, "keep.example.test"))
            .await
            .unwrap();
        let err = apply_node_payload_json(&store, b"not-json")
            .await
            .unwrap_err();
        assert!(matches!(err, StoreApplyError::InvalidPayload(_)));
        assert_eq!(store.get_config_version().await, 3);
        assert!(store.get_server_sync("keep.example.test").is_some());
    }
}
