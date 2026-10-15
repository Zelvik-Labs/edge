//! Regression test for origin load-balancing algorithms.
//!
//! The admin console (`admin-console/src/features/sites/site-settings-fixtures.ts:345-356`)
//! maps the origin-algorithm picker to `reverseProxy.scheduling.code`:
//!   round_robin -> "polling", weighted -> "weight",
//!   ip_hash -> "hash", least_conn -> "least_conn".
//!
//! Previously every non-random code collapsed to `LoadBalancer<RoundRobin>`, so
//! `hash` (IP stickiness) and `least_conn` silently behaved like polling, and
//! the selection key passed at `proxy.rs` was the empty string. This test pins
//! the intended behavior through the production `build_lb` path.

use std::collections::{BTreeSet, HashMap};
use zelvik_edge::config_models::ReverseProxyConfig;
use zelvik_edge::lb_factory::{AnyLoadBalancer, build_lb, peer_origin_id};

fn rp_cfg(code: Option<&str>, ids: &[i64]) -> ReverseProxyConfig {
    let scheduling = match code {
        Some(code) => serde_json::json!({
            "code": code,
            "options": if code == "hash" { serde_json::json!({"hashKey": "ip"}) } else { serde_json::json!({}) }
        }),
        None => serde_json::Value::Null,
    };
    let primary_origins: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            serde_json::json!({
                "id": id,
                "isOn": true,
                "addr": format!("http://127.0.0.1:{}", 9600 + index)
            })
        })
        .collect();
    serde_json::from_value(serde_json::json!({
        "isOn": true,
        "scheduling": scheduling,
        "requestHostType": 0,
        "primaryOrigins": primary_origins
    }))
    .expect("reverse proxy config should parse")
}

fn variant_name(lb: &AnyLoadBalancer) -> &'static str {
    match lb {
        AnyLoadBalancer::RoundRobin(_) => "RoundRobin",
        AnyLoadBalancer::Random(_) => "Random",
        AnyLoadBalancer::Consistent(_) => "Consistent",
        AnyLoadBalancer::LeastConn(_) => "LeastConn",
        // Origins live behind an OriginPool wrapper; the pool type of interest is
        // the primary (as_round_robin() also peels this wrapper).
        AnyLoadBalancer::OriginPool { primary, .. } => variant_name(primary),
    }
}

fn build(code: Option<&str>, ids: &[i64]) -> std::sync::Arc<AnyLoadBalancer> {
    let cfg = rp_cfg(code, ids);
    let (lb, _hc) = build_lb(1, &cfg, 2, &HashMap::new(), false, true);
    lb
}

#[test]
fn scheduling_codes_build_the_intended_algorithm() {
    let cases: &[(Option<&str>, &str)] = &[
        (Some("polling"), "RoundRobin"),
        (Some("weight"), "RoundRobin"),
        (Some("hash"), "Consistent"),
        (Some("least_conn"), "LeastConn"),
        (Some("random"), "Random"),
        (None, "Random"),
        // Unknown code falls back to a deterministic default (round robin).
        (Some("bogus"), "RoundRobin"),
    ];
    for (code, expected) in cases {
        let lb = build(*code, &[11, 12, 13]);
        let got = variant_name(&lb);
        println!("[build] scheduling.code={code:?} -> AnyLoadBalancer::{got}");
        assert_eq!(got, *expected, "scheduling.code={code:?}");
    }
}

#[test]
fn consistent_hash_is_sticky_and_advances_past_a_down_backend() {
    let lb = build(Some("hash"), &[21, 22, 23]);
    assert_eq!(variant_name(&lb), "Consistent");
    // Not a round-robin pool; keyed by client IP.
    assert!(lb.as_round_robin().is_none());

    // Same key -> same backend, every time (IP stickiness).
    let first = lb.select(b"203.0.113.7", 32).expect("backend");
    let seen_first = (0..16)
        .map(|_| lb.select(b"203.0.113.7", 32).expect("backend"))
        .collect::<BTreeSet<_>>();
    assert_eq!(seen_first.len(), 1, "one key must map to one backend");
    assert!(seen_first.contains(&first));

    // Different keys spread across more than one backend.
    let mut spread = BTreeSet::new();
    for key in 0u8..96 {
        spread.insert(lb.select(&[key], 32).expect("backend").to_string());
    }
    println!("[hash] distinct backends over 96 keys = {}", spread.len());
    assert!(spread.len() > 1, "distinct keys must distribute, got {spread:?}");

    // Health fallback must walk the ring to another backend when the first pick
    // is marked down (this is the fixed behavior; the old loop re-issued the same
    // key and spun without advancing).
    let first_id = peer_origin_id(&first);
    let alternate = lb
        .select_with_backup(b"203.0.113.7", 32, |origin_id| origin_id == first_id)
        .expect("fallback backend");
    println!(
        "[hash] down={first_id} first={first:?} fallback={alternate:?}"
    );
    assert_ne!(alternate, first, "must advance past the down backend");
    assert_ne!(peer_origin_id(&alternate), first_id);
}

#[test]
fn least_conn_prefers_the_lowest_inflight_origin() {
    let lb = build(Some("least_conn"), &[810001, 810002, 810003]);
    assert_eq!(variant_name(&lb), "LeastConn");
    assert!(lb.as_round_robin().is_none());

    let counters = zelvik_edge::origin_state::ORIGIN_STATE_MANAGER.inflight_counters();
    // Load two origins; the idle third must be chosen.
    let _g1 = zelvik_edge::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(810001);
    let _g2 = zelvik_edge::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(810002);
    let _g3 = zelvik_edge::origin_state::ORIGIN_STATE_MANAGER.begin_inflight(810002);

    let peer = lb.select(b"", 16).expect("backend");
    println!(
        "[least_conn] inflight 810001={} 810002={} 810003={} -> picked {}",
        counters.inflight(810001),
        counters.inflight(810002),
        counters.inflight(810003),
        peer_origin_id(&peer)
    );
    assert_eq!(peer_origin_id(&peer), 810003);

    // Releasing the guards drains the counters (no leak).
    drop(_g1);
    drop(_g2);
    drop(_g3);
    assert_eq!(counters.inflight(810002), 0);
}

#[test]
fn empty_and_random_still_build_random_pools() {
    for code in [None, Some("random")] {
        let lb = build(code, &[31, 32, 33]);
        assert_eq!(variant_name(&lb), "Random", "code={code:?}");
        assert!(lb.as_round_robin().is_none());
    }
}
