//! Compare the retained listener-set APIs with the per-datagram point query.
//! These are configuration lookup timings, not proxy throughput measurements.
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::collections::HashMap;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use zelvik_edge::config::ConfigStore;
use zelvik_edge::config_models::{HTTP3Policy, ServerConfig};
use zelvik_edge::http3_proxy_manager::Http3ProxyManager;

#[path = "../src/config/h3_ports.rs"]
mod h3_ports;

fn fixture(rt: &Runtime, sites: usize, policy_port: i32) -> (ConfigStore, Arc<Http3ProxyManager>) {
    let store = ConfigStore::new();
    let mut all_servers = Vec::new();
    for index in 0..sites {
        all_servers.push(Arc::new(
            serde_json::from_value::<ServerConfig>(serde_json::json!({
                "id": index+1, "isOn": true,
                "serverNames": [{"name": format!("host{index}.example")}],
                "https": {"isOn": true, "http3Enabled": true, "listen": [{"portRange": "443-444"}]}
            }))
            .expect("valid H3 server fixture"),
        ));
    }
    all_servers.push(Arc::new(
        serde_json::from_value::<ServerConfig>(serde_json::json!({
            "id": sites+1, "isOn": true,
            "serverNames": [{"name": "forward.example@QUIC"}],
            "udp": {"isOn": true, "listen": [{"portRange": "5300"}]}
        }))
        .expect("valid QUIC server fixture"),
    ));
    let policies = HashMap::from([(
        0,
        HTTP3Policy {
            is_on: true,
            port: policy_port,
            address_validation: "always".into(),
            ..Default::default()
        },
    )]);
    rt.block_on(store.update_config(
        1,
        1,
        0,
        0,
        all_servers,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        0,
        1,
        true,
        false,
        HashMap::new(),
        false,
        false,
        String::new(),
        HashMap::new(),
        None,
        false,
        false,
        String::new(),
        false,
        false,
        0,
        false,
        false,
        false,
        String::new(),
        None,
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        policies,
        HashMap::new(),
        HashMap::new(),
        None,
        None,
    ))
    .expect("configuration installs");
    let api_config = Arc::new(zelvik_edge::api_config::ApiConfig {
        rpc_tls: Default::default(),
        rpc_endpoints: Vec::new(),
        rpc_disable_update: true,
        instance_id: "nd_0123456789".into(),
        secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
        comm_mode: String::new(),
        gcloud_node_id: "1".into(),
        access_log_pipeline: Default::default(),
        relay: Default::default(),
        kernel_tuning: Default::default(),
        cache: Default::default(),
    });
    let challenge_key = api_config.challenge_key().expect("fixture challenge key");
    let cert_selector = Arc::new(zelvik_edge::ssl::DynamicCertSelector::new());
    let proxy = zelvik_edge::proxy::EdgeProxy {
        config: Arc::new(store.clone()),
        waf_state: Arc::new(zelvik_edge::firewall::state::WafStateManager::new()),
        api_config,
        challenge_key,
        cert_selector: cert_selector.clone(),
        waf_verifier: Arc::new(zelvik_edge::firewall::verifier::WafVerifier::new(
            &challenge_key,
        )),
        tls_downstream: false,
    };
    let manager = Http3ProxyManager::new(
        store.clone(),
        cert_selector,
        proxy,
        Arc::new(pingora_core::server::configuration::ServerConf::default()),
    );
    (store, manager)
}

fn listener_sets(manager: &Http3ProxyManager, store: &ConfigStore, port: u16) -> Option<bool> {
    if manager.desired_ports_sync().contains(&port) {
        Some(true)
    } else {
        store
            .quic_passthrough_ports_sync()
            .contains(&port)
            .then_some(false)
    }
}

fn udp_demux(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fixture runtime");
    for sites in [1, 1000] {
        for policy_port in [8443, 0] {
            let (store, manager) = fixture(&rt, sites, policy_port);
            let mut group =
                c.benchmark_group(format!("udp_demux/sites{sites}-policy{policy_port}"));
            for port in [443, 8443, 5300, 9999] {
                let expected = match (policy_port, port) {
                    (8443, 8443) | (0, 443) => Some(true),
                    (_, 5300) => Some(false),
                    _ => None,
                };
                assert_eq!(listener_sets(&manager, &store, port), expected);
                assert_eq!(store.af_xdp_demux_http3_enabled_sync(port), expected);
                assert_eq!(
                    store.af_xdp_demux_http3_scan_for_validation_sync(port),
                    expected
                );
                group.bench_with_input(BenchmarkId::new("sets", port), &port, |b, port| {
                    b.iter(|| black_box(listener_sets(&manager, &store, black_box(*port))))
                });
                group.bench_with_input(BenchmarkId::new("point", port), &port, |b, port| {
                    b.iter(|| black_box(store.af_xdp_demux_http3_enabled_sync(black_box(*port))))
                });
                group.bench_with_input(BenchmarkId::new("scan", port), &port, |b, port| {
                    b.iter(|| {
                        black_box(
                            store.af_xdp_demux_http3_scan_for_validation_sync(black_box(*port)),
                        )
                    })
                });
            }
            group.finish();
        }
    }
    let mut group = c.benchmark_group("h3_site_index_build");
    for sites in [1, 1000] {
        for (start, end) in [(443, 444), (0, 65535)] {
            let mut check = h3_ports::Http3SitePorts::default();
            for _ in 0..sites {
                check.insert_range(start, end);
            }
            assert!(check.contains(start) && check.contains(end));
            group.bench_with_input(
                BenchmarkId::new(format!("sites{sites}"), format!("{start}-{end}")),
                &(sites, start, end),
                |b, &(sites, start, end)| {
                    b.iter(|| {
                        let mut index = h3_ports::Http3SitePorts::default();
                        for _ in 0..sites {
                            index.insert_range(black_box(start), black_box(end));
                        }
                        black_box(index)
                    })
                },
            );
        }
    }
    group.finish();
}

fn criterion_config() -> Criterion {
    let config = Criterion::default()
        .sample_size(30)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(2));
    if let Some(output) = std::env::var_os("ZELVIK_UDP_DEMUX_BENCH_OUTPUT") {
        config.output_directory(&std::path::PathBuf::from(output))
    } else {
        config
    }
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = udp_demux
}
criterion_main!(benches);
