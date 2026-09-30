use cloudsidra_edge::compiled::CompiledServerFeaturePlan;
use cloudsidra_edge::config_models::NodeConfigPayload;
use cloudsidra_edge::firewall::state::WafStateManager;

#[test]
fn actual_admin_payload_projected_by_api_parses_and_compiles() {
    let node: NodeConfigPayload =
        serde_json::from_str(include_str!("fixtures/control-plane/site-edge.json")).unwrap();
    assert!(node.enable_ip_lists);
    let server = &node.servers[0];
    let web = server.web.as_ref().unwrap();
    assert_eq!(web.redirect_to_https.as_ref().unwrap().status, 301);
    let compression = web.compression.as_ref().unwrap();
    assert!(compression.gzip.as_ref().unwrap().is_on);
    assert!(compression.brotli.as_ref().unwrap().is_on);
    assert_eq!(web.cc_policy.as_ref().unwrap().requests_per_minute, 30);
    assert_eq!(server.traffic_limit.as_ref().unwrap().downlink_mbps, 8.0);
    assert!(server.traffic_limit.as_ref().unwrap().is_on);
    assert_eq!(web.pages[0].content_type.as_deref(), Some("text/plain"));
    assert_eq!(web.rewrite_rules[0].flags, "i");
    assert_eq!(
        web.request_header_policy
            .as_ref()
            .unwrap()
            .set_headers
            .len(),
        5
    );
    let plan = CompiledServerFeaturePlan::compile(server).unwrap();
    assert_eq!(plan.request_limit().unwrap().rate, 10);
    assert_eq!(plan.request_limit().unwrap().max_conns, 20);
    assert!(plan.has_auth());
    let firewall = web.firewall_policy.as_ref().unwrap();
    let groups = &firewall.inbound.as_ref().unwrap().groups;
    assert_eq!(groups[0].mode, "observe");
    assert!(groups.len() >= 2);
    let compiled_firewall =
        cloudsidra_edge::firewall::compiled::CompiledFirewallPolicy::compile(firewall);
    assert!(compiled_firewall.inbound.is_some());
    let cache = web.cache.as_ref().unwrap().cache_policy.as_ref().unwrap();
    let compiled_cache = cloudsidra_edge::cache::compiled::CompiledCachePolicy::compile_arc(cache);
    assert_eq!(compiled_cache.refs.len(), 2);
    assert_eq!(compiled_cache.refs[0].response_policy.ttl_seconds(), 60);
    assert_eq!(
        cache.options.as_ref().unwrap()["staleWhileRevalidateSeconds"],
        20
    );
    assert!(
        cache
            .options
            .as_ref()
            .unwrap()
            .contains_key("cacheGeneration")
    );
}

#[test]
fn cc_minute_and_cache_write_limits_have_independent_windows_and_scopes() {
    let state = WafStateManager::new();
    let ip = "198.51.100.73".parse().unwrap();
    assert!(state.check_ip_minute_limit(123, ip, 2));
    assert!(state.check_ip_minute_limit(123, ip, 2));
    assert!(!state.check_ip_minute_limit(123, ip, 2));
    assert!(state.check_ip_rate_limit(123, ip, 1));
    assert!(state.check_cache_write_limit(123, ip, 1, 60));
    assert!(!state.check_cache_write_limit(123, ip, 1, 60));
    // Updating a window must rebuild the actual quota, even with equal count.
    assert!(state.check_cache_write_limit(123, ip, 1, 120));
    assert!(state.check_ip_minute_limit(124, ip, 2));
}

#[tokio::test]
async fn shared_certificate_ocsp_is_per_site_and_toggle_keeps_staple() {
    use cloudsidra_edge::{
        config_models::{SSLCertConfig, ServerConfig},
        ssl::{DynamicCertSelector, sync_certs_for_servers},
    };
    use std::sync::Arc;
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id":17,"isOn":true,"isDefault":true,"ocsp":true,
        "certData":include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem"),
        "keyData":include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem"),
        "dnsNames":["a.example","b.example"]
    }))
    .unwrap();
    let make_server = |host: &str, enabled: bool| {
        Arc::new(serde_json::from_value::<ServerConfig>(serde_json::json!({
        "id":if enabled {1}else{2},"isOn":true,"serverNames":[{"name":host}],
        "https":{"isOn":true,"sslPolicy":{"id":1,"isOn":true,"ocsp":enabled,"certs":[cert.clone()]}}
    })).unwrap())
    };
    let selector = DynamicCertSelector::new();
    sync_certs_for_servers(
        &selector,
        std::slice::from_ref(&cert),
        &[
            make_server("a.example", true),
            make_server("b.example", false),
        ],
    )
    .await;
    selector
        .update_ocsp_with_expiry(17, vec![1, 2, 3], chrono::Utc::now().timestamp() + 3600)
        .await
        .unwrap();
    assert_eq!(
        selector
            .resolve_certified_key_for_host("a.example")
            .unwrap()
            .ocsp,
        Some(vec![1, 2, 3])
    );
    assert!(
        selector
            .resolve_certified_key_for_host("b.example")
            .unwrap()
            .ocsp
            .is_none()
    );
    assert!(
        selector
            .export_pair_pem_for_host("b.example")
            .unwrap()
            .2
            .is_empty()
    );
    let mut disabled = cert.clone();
    disabled.ocsp = false;
    sync_certs_for_servers(&selector, &[disabled], &[make_server("a.example", false)]).await;
    assert!(
        selector
            .resolve_certified_key_for_host("a.example")
            .unwrap()
            .ocsp
            .is_none()
    );
    sync_certs_for_servers(&selector, &[cert], &[]).await;
    assert_eq!(
        selector
            .resolve_certified_key_for_host("a.example")
            .unwrap()
            .ocsp,
        Some(vec![1, 2, 3])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn observing_waf_group_cannot_bypass_later_defense() {
    use cloudsidra_edge::{config_models::HTTPFirewallPolicy, firewall};
    use tokio::io::AsyncWriteExt;
    let (mut client, downstream) = tokio::net::UnixStream::pair().unwrap();
    client
        .write_all(b"GET /private HTTP/1.1\r\nHost: site.example\r\nUser-Agent: badbot\r\n\r\n")
        .await
        .unwrap();
    let mut session = pingora::proxy::Session::new_h1(Box::new(
        pingora::protocols::l4::stream::Stream::from(downstream),
    ));
    session.downstream_session.read_request().await.unwrap();
    let group = |id: i64, mode: &str, status: i64| serde_json::json!({"id":id,"isOn":true,"mode":mode,"sets":[{"id":id,"isOn":true,"connector":"and","rules":[{"param":"${userAgent}","operator":"contains","value":"badbot"}],"actions":[{"code":"block","options":{"status":status,"body":"blocked"}}]}]});
    let policy:HTTPFirewallPolicy=serde_json::from_value(serde_json::json!({"id":1,"isOn":true,"mode":"defense","inbound":{"isOn":true,"groups":[group(1,"observe",403),group(2,"defense",451)]}})).unwrap();
    let raw = firewall::evaluate_policy(&policy, &session, &[], "http").unwrap();
    let compiled = firewall::compiled::evaluate_compiled_policy(
        &firewall::compiled::CompiledFirewallPolicy::compile(&policy),
        &session,
        &[],
        "http",
    )
    .unwrap();
    for matched in [raw, compiled] {
        assert_eq!(matched.group_id, 2);
        assert!(!matched.observe_only);
        assert!(matches!(
            matched.action,
            firewall::ActionResponse::Block { status: 451, .. }
        ));
    }
}

#[test]
fn unsupported_rewrite_flags_are_rejected_instead_of_ignored() {
    use cloudsidra_edge::config_models::HTTPRewriteRule;
    assert!(serde_json::from_value::<HTTPRewriteRule>(serde_json::json!({"flags":"ig"})).is_ok());
    assert!(serde_json::from_value::<HTTPRewriteRule>(serde_json::json!({"flags":"z"})).is_err());
}

#[tokio::test]
async fn ocsp_received_before_certificate_is_retained_and_expired_staples_are_omitted() {
    use cloudsidra_edge::{
        config_models::SSLCertConfig,
        ssl::{DynamicCertSelector, sync_certs},
    };
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id":19,"isOn":true,"isDefault":true,"ocsp":true,
        "certData":include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem"),
        "keyData":include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem"),
        "dnsNames":["early.example"]
    }))
    .unwrap();
    let selector = DynamicCertSelector::new();
    selector
        .update_ocsp_with_expiry(19, vec![4, 5, 6], chrono::Utc::now().timestamp() + 3600)
        .await
        .unwrap();
    sync_certs(&selector, std::slice::from_ref(&cert)).await;
    assert_eq!(
        selector
            .resolve_certified_key_for_host("early.example")
            .unwrap()
            .ocsp,
        Some(vec![4, 5, 6])
    );
    selector
        .update_ocsp_with_expiry(19, vec![7, 8, 9], chrono::Utc::now().timestamp() - 1)
        .await
        .unwrap();
    assert!(
        selector
            .resolve_certified_key_for_host("early.example")
            .unwrap()
            .ocsp
            .is_none()
    );
    assert!(
        selector
            .export_pair_pem_for_host("early.example")
            .unwrap()
            .2
            .is_empty()
    );
    sync_certs(&selector, &[cert]).await;
    assert!(
        selector
            .resolve_certified_key_for_host("early.example")
            .unwrap()
            .ocsp
            .is_none()
    );
}

#[tokio::test]
async fn inline_certificate_site_policy_matches_material_after_synthetic_id_assignment() {
    use cloudsidra_edge::{
        config_models::{SSLCertConfig, ServerConfig},
        ssl::{DynamicCertSelector, sync_certs_for_servers},
    };
    use std::sync::Arc;
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id":-1,"isOn":true,"isDefault":true,"ocsp":true,
        "certData":include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem"),
        "keyData":include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem"),
        "dnsNames":["inline.example"]
    }))
    .unwrap();
    let mut original = cert.clone();
    original.id = 0;
    let server = Arc::new(
        serde_json::from_value::<ServerConfig>(serde_json::json!({
            "id":20,"isOn":true,"serverNames":[{"name":"inline.example"}],
            "https":{"isOn":true,"sslPolicy":{"id":20,"isOn":true,"ocsp":false,"certs":[original]}}
        }))
        .unwrap(),
    );
    let selector = DynamicCertSelector::new();
    selector
        .update_ocsp_with_expiry(-1, vec![1, 2, 3], chrono::Utc::now().timestamp() + 3600)
        .await
        .unwrap();
    sync_certs_for_servers(&selector, &[cert], &[server]).await;
    assert!(
        selector
            .resolve_certified_key_for_host("inline.example")
            .unwrap()
            .ocsp
            .is_none()
    );
}
