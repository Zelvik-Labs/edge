use zelvik_edge::compiled::CompiledServerFeaturePlan;
use zelvik_edge::config_models::NodeConfigPayload;
use zelvik_edge::firewall::state::WafStateManager;

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
        zelvik_edge::firewall::compiled::CompiledFirewallPolicy::compile(firewall);
    assert!(compiled_firewall.inbound.is_some());
    let cache = web.cache.as_ref().unwrap().cache_policy.as_ref().unwrap();
    let compiled_cache = zelvik_edge::cache::compiled::CompiledCachePolicy::compile_arc(cache);
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
    use zelvik_edge::{
        config_models::{SSLCertConfig, ServerConfig},
        ssl::{DynamicCertSelector, sync_certs_for_servers},
    };
    use std::sync::Arc;
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id":17,"isOn":true,"isDefault":true,"ocsp":true,
        "certData":include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem"),
        "keyData":include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem"),
        "dnsNames":["a.openrusty.org","b.openrusty.org"]
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
            make_server("a.openrusty.org", true),
            make_server("b.openrusty.org", false),
        ],
    )
    .await;
    selector
        .update_ocsp_with_expiry(17, vec![1, 2, 3], chrono::Utc::now().timestamp() + 3600)
        .await
        .unwrap();
    assert_eq!(
        selector
            .resolve_certified_key_for_host("a.openrusty.org")
            .unwrap()
            .ocsp,
        Some(vec![1, 2, 3])
    );
    assert!(
        selector
            .resolve_certified_key_for_host("b.openrusty.org")
            .unwrap()
            .ocsp
            .is_none()
    );
    assert!(
        selector
            .export_pair_pem_for_host("b.openrusty.org")
            .unwrap()
            .2
            .is_empty()
    );
    let mut disabled = cert.clone();
    disabled.ocsp = false;
    sync_certs_for_servers(&selector, &[disabled], &[make_server("a.openrusty.org", false)]).await;
    assert!(
        selector
            .resolve_certified_key_for_host("a.openrusty.org")
            .unwrap()
            .ocsp
            .is_none()
    );
    sync_certs_for_servers(&selector, &[cert], &[]).await;
    assert_eq!(
        selector
            .resolve_certified_key_for_host("a.openrusty.org")
            .unwrap()
            .ocsp,
        Some(vec![1, 2, 3])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn observing_waf_group_cannot_bypass_later_defense() {
    use zelvik_edge::{config_models::HTTPFirewallPolicy, firewall};
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
    use zelvik_edge::config_models::HTTPRewriteRule;
    assert!(serde_json::from_value::<HTTPRewriteRule>(serde_json::json!({"flags":"ig"})).is_ok());
    assert!(serde_json::from_value::<HTTPRewriteRule>(serde_json::json!({"flags":"z"})).is_err());
}

#[tokio::test]
async fn ocsp_received_before_certificate_is_retained_and_expired_staples_are_omitted() {
    use zelvik_edge::{
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
    use zelvik_edge::{
        config_models::{SSLCertConfig, ServerConfig},
        ssl::{DynamicCertSelector, sync_certs_for_servers},
    };
    use std::sync::Arc;
    let cert: SSLCertConfig = serde_json::from_value(serde_json::json!({
        "id":-1,"isOn":true,"isDefault":true,"ocsp":true,
        "certData":include_str!("../pingora-main/pingora-core/examples/keys/server/cert.pem"),
        "keyData":include_str!("../pingora-main/pingora-core/examples/keys/server/key.pem"),
        "dnsNames":["inline.openrusty.org"]
    }))
    .unwrap();
    let mut original = cert.clone();
    original.id = 0;
    let server = Arc::new(
        serde_json::from_value::<ServerConfig>(serde_json::json!({
            "id":20,"isOn":true,"serverNames":[{"name":"inline.openrusty.org"}],
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
            .resolve_certified_key_for_host("inline.openrusty.org")
            .unwrap()
            .ocsp
            .is_none()
    );
}

/// Console/control-plane replace rules are `{name, newValue}` only —
/// `admin-console/src/features/sites/site-settings-fixtures.ts` and
/// `tests/fixtures/control-plane/site-edge.json` never carry `oldValue`, so the field
/// deserializes to `""`. An empty `oldValue` must mean "set the whole header value";
/// `str::replace("", new)` would instead splice `new` between every character
/// (`"value1"` -> `"newvnewanewlnewunewenew1new"`). These assertions check the *final
/// header value* produced by every consumer of `replace_headers`, not just that the
/// payload deserializes.
#[test]
fn replace_header_rule_value_semantics_for_console_payloads() {
    use std::collections::HashMap;
    use zelvik_edge::config_models::HTTPHeaderPolicy;
    use zelvik_edge::headers::{
        CompiledHeaderPolicy, RequestTemplateVars, apply_compiled_response_header_policy,
        apply_response_header_policy, apply_response_header_policy_to_map,
    };

    let vars = RequestTemplateVars {
        scheme: "https",
        method: "GET",
        host: "site.example",
        request_uri: "/",
        path: "/",
        query: "",
        port: "443",
        referer: "",
        referer_block: "",
        user_agent: "contract-test",
        content_type: "",
        remote_addr: "198.51.100.7",
    };

    let policy = |rule: serde_json::Value| -> HTTPHeaderPolicy {
        serde_json::from_value(serde_json::json!({ "isOn": true, "replaceHeaders": [rule] }))
            .unwrap()
    };

    // Applies the policy through every `replace_headers` consumer (raw response path,
    // compiled response path, legacy map path) and collects the resulting values. The
    // three must agree; an `Option<String>` of `None` means the header is absent.
    let apply_all = |policy: &HTTPHeaderPolicy, name: &str, initial: Option<&str>| {
        let mut raw = pingora_http::ResponseHeader::build(200u16, None).unwrap();
        let mut compiled = pingora_http::ResponseHeader::build(200u16, None).unwrap();
        let mut map = HashMap::new();
        if let Some(value) = initial {
            raw.insert_header(name.to_string(), value).unwrap();
            compiled.insert_header(name.to_string(), value).unwrap();
            map.insert(name.to_lowercase(), value.to_string());
        }
        apply_response_header_policy(&mut raw, policy, &vars, 200, "GET", "site.example");
        apply_compiled_response_header_policy(
            &mut compiled,
            &CompiledHeaderPolicy::compile(policy),
            &vars,
            200,
            "GET",
            "site.example",
        );
        apply_response_header_policy_to_map(&mut map, policy);
        [
            raw.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            compiled
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            map.get(&name.to_lowercase()).cloned(),
        ]
    };

    // (a) Missing `oldValue` (the only shape the console can produce): set the whole
    //     value to `newValue`; no per-character splicing.
    let no_old_value =
        policy(serde_json::json!({ "isOn": true, "name": "X-Old", "newValue": "new" }));
    for got in apply_all(&no_old_value, "X-Old", Some("value1")) {
        assert_eq!(
            got.as_deref(),
            Some("new"),
            "empty oldValue must set the whole header value"
        );
    }

    // (b) A non-empty `oldValue` keeps substring-replace semantics.
    let substring = policy(
        serde_json::json!({ "isOn": true, "name": "X-Old", "oldValue": "o", "newValue": "0" }),
    );
    for got in apply_all(&substring, "X-Old", Some("foo")) {
        assert_eq!(
            got.as_deref(),
            Some("f00"),
            "non-empty oldValue must still substring-replace"
        );
    }

    // (c) A replace rule never creates a header that was not already present.
    for got in apply_all(&no_old_value, "X-Old", None) {
        assert_eq!(got, None, "a replace rule must not add a missing header");
    }
}

/// The request-side consumers must apply `requestHeaderPolicy.replaceHeaders` too. The
/// fixture's rule (`site-edge.json`, `{"isOn":true,"name":"X-Old","newValue":"new"}`,
/// no `oldValue`) previously reached upstream untouched, because the request policy only
/// walked the delete/set/add arrays. These assertions check the *outgoing request*
/// header value produced by both request-side consumers.
#[test]
fn request_header_replace_rule_from_fixture_sets_whole_value_upstream() {
    use pingora_http::RequestHeader;
    use zelvik_edge::config_models::HTTPHeaderPolicy;
    use zelvik_edge::headers::{
        CompiledHeaderPolicy, RequestTemplateVars,
        apply_compiled_request_header_policy_to_upstream, apply_request_header_policy_to_upstream,
    };

    let node: NodeConfigPayload =
        serde_json::from_str(include_str!("fixtures/control-plane/site-edge.json")).unwrap();
    let policy = node.servers[0]
        .web
        .as_ref()
        .unwrap()
        .request_header_policy
        .clone()
        .unwrap();
    let rule = policy
        .replace_headers
        .first()
        .expect("fixture requestHeaderPolicy carries a replaceHeaders rule");
    assert_eq!(rule.name, "X-Old");
    assert_eq!(rule.old_value, "", "the fixture rule carries no oldValue");
    assert_eq!(rule.new_value, "new");

    let vars = RequestTemplateVars {
        scheme: "https",
        method: "GET",
        host: "site.example",
        request_uri: "/",
        path: "/",
        query: "",
        port: "443",
        referer: "",
        referer_block: "",
        user_agent: "contract-test",
        content_type: "",
        remote_addr: "198.51.100.7",
    };

    // Applies the policy through both request-side consumers (raw and compiled) and
    // returns the value of `name` on the outgoing request (`None` = header absent).
    let apply_request = |policy: &HTTPHeaderPolicy, name: &str, initial: Option<&str>| {
        let mut raw = RequestHeader::build("GET", b"/", None).unwrap();
        let mut compiled = RequestHeader::build("GET", b"/", None).unwrap();
        if let Some(value) = initial {
            raw.insert_header(name.to_string(), value).unwrap();
            compiled.insert_header(name.to_string(), value).unwrap();
        }
        apply_request_header_policy_to_upstream(&mut raw, policy, &vars);
        apply_compiled_request_header_policy_to_upstream(
            &mut compiled,
            &CompiledHeaderPolicy::compile(policy),
            &vars,
        );
        [
            raw.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            compiled
                .headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        ]
    };

    // (a) The fixture's rule has no `oldValue`: set the whole value to `newValue`.
    for got in apply_request(&policy, "X-Old", Some("value1")) {
        assert_eq!(
            got.as_deref(),
            Some("new"),
            "requestHeaderPolicy.replaceHeaders must set the outgoing header to newValue"
        );
    }

    // (b) A non-empty `oldValue` still substring-replaces on the request side.
    let mut substring = policy.clone();
    substring.replace_headers[0].old_value = "o".to_string();
    substring.replace_headers[0].new_value = "0".to_string();
    for got in apply_request(&substring, "X-Old", Some("foo")) {
        assert_eq!(
            got.as_deref(),
            Some("f00"),
            "non-empty oldValue must still substring-replace on the request side"
        );
    }

    // (c) A replace rule never adds a request header that was not already present.
    for got in apply_request(&policy, "X-Old", None) {
        assert_eq!(
            got, None,
            "a replace rule must not add a missing request header"
        );
    }
}

/// Cross-repo contract guard: every JSON key the console / control-plane emit for a site
/// server must be modelled by an Edge serde struct. serde silently drops a key that
/// matches no field — no error, no warning, no log — so an unmodelled key looks like it
/// was applied while never being consumed. That is exactly how `web.optimization.isOn`
/// and the origin-level `proxyProtocol` / `requestHostType` / `requestHostExcludingPort`
/// drifted (admin-console#9).
///
/// The check is a round-trip: deserialize the fixture into `NodeConfigPayload`, serialize
/// it back, and require the fixture's key set to be a *subset* of the round-tripped key
/// set at every path. A fixture key with no counterpart in the round-trip is one that no
/// field claims; the model can only re-emit a key it deserialized. Fields the fixture
/// omits but the model defaults (e.g. `telemetry`) only ever appear in the round-trip, so
/// the subset direction is the correct one.
///
/// Free-form JSON subtrees (fields typed `serde_json::Value` / `HashMap<String, Value>`)
/// hold keys Edge deliberately does not model — they are configuration *data*, not a
/// schema, and they round-trip verbatim. They are listed in `FREE_JSON_SUBTREES` and left
/// unvisited, so this guard never demands a struct field for their contents.
#[test]
fn fixture_keys_are_all_modelled_by_edge_config() {
    // Root-relative paths whose *contents* are opaque free-form JSON — intentionally not
    // modelled, consumed by the feature that owns the subtree:
    //   - `reverseProxy.scheduling.options`: `SchedulingConfig.options: Value`, carries the
    //     scheduling algorithm's own parameters (e.g. `{hashKey}`).
    //   - `web.auth.policyRefs[].authPolicy.params`: `AuthPolicyConfig.params: Value`, the
    //     per-auth-type parameter bag (users, realm, …).
    //   - `web.cache.cachePolicy.options`: `WebCacheConfig.options: Option<HashMap<String,
    //     Value>>`, cache tuning knobs interpreted by name.
    //   - `web.firewallPolicy.inbound.groups[].sets[].actions[].options`:
    //     `WAFActionConfig.options: Value`, per-action payload (`{status, body, …}`).
    const FREE_JSON_SUBTREES: &[&str] = &[
        "/servers[]/reverseProxy/scheduling/options",
        "/servers[]/web/auth/policyRefs[]/authPolicy/params",
        "/servers[]/web/cache/cachePolicy/options",
        "/servers[]/web/firewallPolicy/inbound/groups[]/sets[]/actions[]/options",
    ];

    // Keys this guard still flags but that admin-console#9 did NOT remove. The control-plane
    // cache projection writes `cachePolicy.enabled` from the cache cluster policy's `Enabled`
    // (`internal/gcloud/policies.go`). Edge models the equivalent *site* gate as
    // `web.cache.isOn` (`WebCacheConfig.is_on`, set from the same `row.Enabled`) and drops
    // this duplicate on every payload path — both the site `cachePolicy` and the node
    // `httpCachePolicies` use `HTTPCachePolicy`, which has no `enabled` field. Kept and
    // reported rather than deleted: removing it touches control-plane cache wiring, and
    // whether Edge should honour a *disabled* cluster policy is a separate question. Every
    // entry here must still be observed below, so a stale entry fails loudly.
    const REPORTED_UNMODELLED: &[&str] = &["/servers[]/web/cache/cachePolicy/enabled"];

    let raw = include_str!("fixtures/control-plane/site-edge.json");
    let original: serde_json::Value = serde_json::from_str(raw).unwrap();
    let node: NodeConfigPayload = serde_json::from_str(raw).unwrap();
    let round = serde_json::to_value(&node).unwrap();

    let mut missing = Vec::new();
    collect_unmodelled_keys(&original, &round, "", FREE_JSON_SUBTREES, &mut missing);
    for reported in REPORTED_UNMODELLED {
        assert!(
            missing.iter().any(|path| path == reported),
            "stale allowlist entry: {reported} is now modelled by Edge — drop it"
        );
    }
    let unexpected: Vec<&String> = missing
        .iter()
        .filter(|path| !REPORTED_UNMODELLED.contains(&path.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "fixture carries keys Edge does not model; serde drops them silently: {unexpected:#?}"
    );
}

fn collect_unmodelled_keys(
    original: &serde_json::Value,
    round: &serde_json::Value,
    path: &str,
    free_json: &[&str],
    out: &mut Vec<String>,
) {
    match original {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let child = format!("{path}/{key}");
                if free_json.contains(&child.as_str()) {
                    continue; // opaque subtree: contents are data, not a schema
                }
                match round.get(key) {
                    Some(round_value) => {
                        collect_unmodelled_keys(value, round_value, &child, free_json, out);
                    }
                    None => out.push(child),
                }
            }
        }
        serde_json::Value::Array(items) => {
            if let serde_json::Value::Array(round_items) = round
                && round_items.len() == items.len()
            {
                let child = format!("{path}[]");
                for (value, round_value) in items.iter().zip(round_items) {
                    collect_unmodelled_keys(value, round_value, &child, free_json, out);
                }
            }
        }
        _ => {}
    }
}
