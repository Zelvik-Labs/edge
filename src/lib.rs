/// Product release version in CalVer `YY.M.W[.P]`.
///
/// `ZELVIK_RELEASE_VERSION` lets the release build inject a four-segment
/// patch version (e.g. `26.10.2.1`), which Cargo's three-segment package
/// version cannot express; otherwise this falls back to `CARGO_PKG_VERSION`.
pub const RELEASE_VERSION: &str = match option_env!("ZELVIK_RELEASE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub mod pb {
    tonic::include_proto!("pb");
}

pub mod adaptive_cpu;
pub mod api_config;
pub mod auth;
pub mod bounded_regex_cache;
pub mod cache;
pub mod cache_hybrid;
pub mod cache_manager;
pub mod client_agent;
pub mod client_ip;
pub mod compiled;
pub mod config;
pub mod config_apply;
pub mod config_models;
pub mod contracts;
pub mod firewall;
pub mod geo;
pub mod h3_downstream;
pub mod headers;
pub mod health_manager;
pub mod http3_proxy_manager;
pub mod http_proxy_manager;
pub mod i18n;
pub mod kernel_syn_defense;
pub mod kernel_tuning;
pub mod l4_connection_registry;
pub mod l4_defense;
pub mod lb_factory;
pub mod local_firewall;
pub mod log_uploader;
pub mod logging;
pub mod memory_governor;
pub mod memory_plan;
pub mod memory_reclaim;
pub mod memory_shed;
pub mod memory_ticket;
pub mod metrics;
pub mod net_bind;
#[cfg(feature = "node-control")]
pub mod node_control;
pub mod node_upgrade;
pub mod origin_h3;
pub mod origin_h3_pool;
pub mod origin_h3_state;
pub mod origin_state;
pub mod oss_origin;
pub mod pages;
pub mod paths;
pub mod perf_monitor;
pub mod pipeline_metrics;
pub mod process_log_policy;
pub mod probe;
pub mod proxy;
pub mod proxy_protocol;
pub mod quic_cc;
pub mod quic_probe;
pub mod quic_transport;
pub mod quic_udp_demux;
pub mod resource_budget;
pub mod rewrite;
pub mod routing;
pub mod rpc;
pub mod runtime_mode;
pub mod special_defense;
pub mod ssl;
pub mod tcp_proxy;
pub mod tls_crypto;
pub mod toa;
#[cfg(any(test, target_os = "linux"))]
pub mod transport_clock;
pub mod udp_proxy;
pub mod unsupported;
pub mod utils;
pub mod xdp;
pub mod xdp_auto_config;
pub mod xdp_config_wizard;
pub mod xdp_netdev_tuning;

pub mod telemetry;
pub mod traffic_quota;
pub mod traffic_shaping;

#[cfg(test)]
mod version_tests {
    /// CalVer `YY.M.W[.P]` — patch segment optional, must be >= 1 when present.
    fn parse_calver(v: &str) -> Option<(u32, u32, u32, u32)> {
        let v = v.trim().trim_start_matches('v');
        let parts: Vec<&str> = v.split('.').collect();
        if !(parts.len() == 3 || parts.len() == 4) {
            return None;
        }
        let n = |s: &str| s.parse::<u32>().ok();
        let (yy, m, w) = (n(parts[0])?, n(parts[1])?, n(parts[2])?);
        if parts[0].len() != 2 || !(1..=12).contains(&m) || !(1..=5).contains(&w) {
            return None;
        }
        let p = if parts.len() == 4 { n(parts[3])? } else { 0 };
        if parts.len() == 4 && p == 0 {
            return None;
        }
        Some((yy, m, w, p))
    }

    /// The injected product version may only extend the Cargo package version
    /// with a patch segment; a different base means the two sources drifted.
    #[test]
    fn release_version_extends_cargo_base() {
        let cargo = env!("CARGO_PKG_VERSION");
        let release = super::RELEASE_VERSION;
        assert!(
            release == cargo || release.starts_with(&format!("{cargo}.")),
            "RELEASE_VERSION {release:?} must equal or extend Cargo version {cargo:?}"
        );
    }

    #[test]
    fn release_version_is_valid_calver() {
        let v = super::RELEASE_VERSION;
        assert!(parse_calver(v).is_some(), "RELEASE_VERSION {v:?} must be CalVer YY.M.W[.P]");
        assert!(parse_calver("26.10.2.1").is_some());
        assert!(parse_calver("26.10.2-beta.1").is_none());
        assert!(parse_calver("26.10.6").is_none());
    }
}
