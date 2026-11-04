//! Runtime integration of per-deployment origin plans with production LB selection.
use crate::config_models::ReverseProxyConfig;
use crate::lb_factory::{BackendExtension, build_lb, peer_origin_id};
use crate::origin_route::unix_now;
use std::collections::HashMap;

fn configuration() -> ReverseProxyConfig {
    let now = unix_now();
    serde_json::from_value(serde_json::json!({
        "isOn":true,
        "originRoute":{
            "siteId":"site-a", "deploymentId":"dep-a", "revision":1,
            "mode":"smart_shield", "role":"ingress", "validUntil":now+600,
            "cacheNamespace":"version-one", "selectedPathId":"hk", "holdUntil":0,
            "paths":[
                {"id":"hk", "kind":"shield", "priority":1, "ready":true,
                 "validUntil":now+600, "observedAt":now, "ttfbMicros":10000,
                 "origins":[{"id":910001,"weight":1,"addr":"https://192.0.2.1:443",
                    "requestHost":"content.example", "tlsServerName":"shield-hk.example",
                    "originAuthentication":{"deploymentId":"dep-a","token":"0123456789abcdef0123456789abcdef"}}]},
                {"id":"sg", "kind":"shield", "priority":2, "ready":true,
                 "validUntil":now+600, "observedAt":now, "ttfbMicros":7000,
                 "origins":[{"id":910002,"weight":1,"addr":"https://192.0.2.2:443",
                    "requestHost":"content.example", "tlsServerName":"shield-sg.example",
                    "originAuthentication":{"deploymentId":"dep-a","token":"0123456789abcdef0123456789abcdef"}}]}
            ]
        }
    })).unwrap()
}

#[test]
fn managed_paths_use_fresh_quality_then_explicit_backup_without_reviving_failed_origins() {
    let config = configuration();
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    assert!(pool.uses_object_key());
    let preferred = pool.select_with_backup(b"object-a", 32, |_| false).unwrap();
    assert_eq!(peer_origin_id(&preferred), 910002);
    let alternate = pool
        .select_with_backup(b"object-a", 32, |id| id == 910002)
        .unwrap();
    assert_eq!(peer_origin_id(&alternate), 910001);
    assert!(pool.select_with_backup(b"object-a", 32, |_| true).is_none());
    let extension = preferred.ext.get::<BackendExtension>().unwrap();
    assert_eq!(extension.host, "content.example");
    assert_eq!(extension.tls_server_name, "shield-sg.example");
    assert!(!format!("{:?}", extension).contains("0123456789abcdef0123456789abcdef"));
}

#[test]
fn stale_metrics_use_static_priority_and_expired_candidates_are_never_selected() {
    let mut config = configuration();
    let now = unix_now();
    let route = config.origin_route.as_mut().unwrap();
    for path in &mut route.paths {
        path.observed_at = Some(now - 300);
    }
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    assert_eq!(
        peer_origin_id(&pool.select(b"object-a", 32).unwrap()),
        910001
    );
    config.origin_route.as_mut().unwrap().paths[0].valid_until = now - 1;
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    assert_eq!(
        peer_origin_id(&pool.select(b"object-a", 32).unwrap()),
        910002
    );
}

#[test]
fn object_affinity_is_stable_for_a_multi_node_shield_pool() {
    let mut config = configuration();
    let route = config.origin_route.as_mut().unwrap();
    route.mode = crate::origin_route::RouteMode::FixedShield;
    let mut extra = route.paths[0].origins[0].clone();
    extra.id = 910003;
    extra.addr = Some(crate::config_models::FlexibleAddr::String(
        "https://192.0.2.3:443".into(),
    ));
    route.paths[0].origins.push(extra);
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    let first = pool.select(b"object-a", 32).unwrap();
    for _ in 0..32 {
        assert_eq!(pool.select(b"object-a", 32).unwrap(), first);
    }
}

#[tokio::test]
async fn origin_route_evidence_only_reports_published_runtime_identity() {
    use std::sync::Arc;
    let config = configuration();
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    let store = crate::config::ConfigStore::new();
    assert!(store.loaded_origin_routes().as_array().unwrap().is_empty());
    let server: crate::config_models::ServerConfig = serde_json::from_value(serde_json::json!({
        "id":910000,"isOn":true,"serverNames":[{"name":"content.example","type":"full"}]
    }))
    .unwrap();
    let server = Arc::new(server);
    store
        .replace_all_servers(
            vec![Arc::clone(&server)],
            HashMap::from([("content.example".into(), server)]),
            HashMap::from([("content.example".into(), Arc::clone(&pool))]),
            HashMap::from([(910000, pool)]),
        )
        .await
        .unwrap();
    let evidence = store.loaded_origin_routes();
    assert_eq!(evidence.as_array().unwrap().len(), 1);
    assert_eq!(evidence[0]["serverId"], 910000);
    assert_eq!(evidence[0]["siteId"], "site-a");
    assert_eq!(evidence[0]["deploymentId"], "dep-a");
    assert_eq!(evidence[0]["revision"], 1);
    assert_eq!(evidence[0]["role"], "ingress");
    let mut pending = config;
    pending.origin_route.as_mut().unwrap().revision = 2;
    assert_eq!(store.loaded_origin_routes()[0]["revision"], 1);
    assert!(
        !evidence
            .to_string()
            .contains("0123456789abcdef0123456789abcdef")
    );
}

#[test]
fn origin_observations_survive_lease_renewal_but_never_cross_execution_revision() {
    let mut config = configuration();
    let (pool, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    let observer = pool.origin_observer(910001).unwrap();
    observer.record(std::time::Duration::from_micros(10000), true);
    config.origin_route.as_mut().unwrap().valid_until += 60;
    let (renewed, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    assert!(std::sync::Arc::ptr_eq(
        &observer,
        &renewed.origin_observer(910001).unwrap()
    ));
    config.origin_route.as_mut().unwrap().revision += 1;
    let (changed, _) = build_lb(910000, &config, 1, &HashMap::new(), false, true);
    assert!(!std::sync::Arc::ptr_eq(
        &observer,
        &changed.origin_observer(910001).unwrap()
    ));
    assert!(
        changed
            .origin_path_observations(910000, unix_now())
            .is_empty()
    );
}

#[test]
fn active_origin_cadence_survives_lease_renewals_and_revision_changes_reset_execution_identity() {
    let mut config = configuration();
    let (pool, _) = build_lb(910010, &config, 1, &HashMap::new(), false, true);
    let now = unix_now();
    let jobs = pool.take_active_origin_probes(now);
    assert_eq!(jobs.len(), 2);
    drop(jobs);
    config.origin_route.as_mut().unwrap().valid_until += 60;
    let (renewed, _) = build_lb(910010, &config, 1, &HashMap::new(), false, true);
    assert!(
        renewed.take_active_origin_probes(now + 1).is_empty(),
        "lease renewal must not increase the sampling rate"
    );
    config.origin_route.as_mut().unwrap().revision += 1;
    let (changed, _) = build_lb(910010, &config, 1, &HashMap::new(), false, true);
    assert_eq!(changed.take_active_origin_probes(now + 1).len(), 2);
}
