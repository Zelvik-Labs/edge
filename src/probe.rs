//! Active TCP-ping (tcping) probing.
//!
//! The control plane pushes a per-node plan through `nodeJSON.runtime.probe`
//! (see [`crate::config_models::ProbePlan`]). Each target is probed on its
//! provider's cadence; every cycle produces one `probe_observation` telemetry
//! batch. There is no local target list — probing follows the pushed plan only.
//!
//! A plan is applied atomically: bumping the generation retires the previous
//! target tasks, so a removed target stops probing immediately.

use crate::config_models::{ProbePlan, ProbeProviderPlan, ProbeTargetPlan};
use crate::pb;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};
use tracing::{debug, warn};

/// TCP SYNs per cycle. Loss and jitter are computed over these samples.
const SAMPLES_PER_CYCLE: u32 = 3;
/// Hard ceiling on concurrently probed targets; a plan beyond this is trimmed
/// (and logged) rather than allowed to spawn unbounded tasks.
const MAX_TARGETS: usize = 4096;
const DEFAULT_INTERVAL_SECS: u32 = 60;
const DEFAULT_TIMEOUT_MS: u32 = 3000;
const MIN_INTERVAL_SECS: u32 = 5;
const MAX_INTERVAL_SECS: u32 = 86400;
const MIN_TIMEOUT_MS: u32 = 100;
const MAX_TIMEOUT_MS: u32 = 60000;

/// Monotonic plan generation. A target task exits once it no longer matches.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static TASKS: LazyLock<parking_lot::Mutex<Vec<JoinHandle<()>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));
/// The plan currently applied, compared by value so an identical push (the
/// control plane re-sends nodeJSON on any unrelated config change) does not
/// restart running probes.
static APPLIED_PLAN: LazyLock<parking_lot::Mutex<Option<ProbePlan>>> =
    LazyLock::new(|| parking_lot::Mutex::new(None));

/// apply_plan replaces the running probe set. `None` (or `enabled = false`)
/// stops every probe — the control plane removes the key when a node's cluster
/// has probing off or no bindings. An unchanged plan is a no-op: nodeJSON is
/// rebuilt for every unrelated config change, and restarting every target
/// would reset its cadence each time.
pub fn apply_plan(plan: Option<ProbePlan>) {
    let plan = plan.filter(|p| p.enabled);
    let mut applied = APPLIED_PLAN.lock();
    if *applied == plan {
        return;
    }
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let mut tasks = TASKS.lock();
    for handle in tasks.drain(..) {
        handle.abort();
    }
    let plan = match plan {
        Some(plan) if plan.enabled => plan,
        _ => {
            debug!("probe plan cleared");
            *applied = None;
            return;
        }
    };
    let mut spawned = 0usize;
    let mut trimmed = 0usize;
    for provider in &plan.providers {
        // One shared provider per target task: the cadence and id are constant
        // across a provider's targets, so the plan is not cloned per target.
        let provider = Arc::new(provider.clone());
        for target in &provider.targets {
            // Identity and family come from the control plane plan: an Edge row
            // must carry the exact check/cluster the quality view authorizes.
            if target.kind != "tcp"
                || target.check_id.is_empty()
                || target.cluster_id.is_empty()
                || target.host.trim().is_empty()
                || target.port == 0
            {
                warn!(target = %target.id, "network quality target rejected: incomplete plan identity");
                continue;
            }
            if spawned >= MAX_TARGETS {
                trimmed += 1;
                continue;
            }
            tasks.push(tokio::spawn(run_target(
                generation,
                Arc::clone(&provider),
                target.clone(),
            )));
            spawned += 1;
        }
    }
    if trimmed > 0 {
        warn!(
            trimmed,
            "probe plan exceeded target ceiling; extra targets ignored"
        );
    }
    *applied = Some(plan);
    debug!(targets = spawned, "probe plan applied");
}

/// run_target probes one target every interval until the plan generation
/// changes or the task is aborted.
async fn run_target(generation: u64, provider: Arc<ProbeProviderPlan>, target: ProbeTargetPlan) {
    let interval = Duration::from_secs(clamp_interval(provider.interval_seconds) as u64);
    let timeout_dur = Duration::from_millis(clamp_timeout(provider.timeout_ms) as u64);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            break;
        }
        let observation = probe_once(&provider, &target, timeout_dur).await;
        let batch = pb::ProbeObservationBatch {
            observations: vec![observation],
        };
        if let Err(err) = crate::telemetry::enqueue("probe_observation", &batch, 0, false).await {
            warn!(target = %target.id, error = %err, "probe observation enqueue failed");
        }
    }
}

/// probe_once runs one cycle of TCP SYNs and folds the samples into a single
/// observation: latency is the mean of successful RTTs, jitter the mean
/// absolute difference between consecutive RTTs, loss the failed fraction.
async fn probe_once(
    provider: &ProbeProviderPlan,
    target: &ProbeTargetPlan,
    timeout_dur: Duration,
) -> pb::ProbeObservation {
    let mut sent = 0u32;
    let mut received = 0u32;
    let mut rtts: Vec<f64> = Vec::with_capacity(SAMPLES_PER_CYCLE as usize);
    let mut last_error = String::new();
    for _ in 0..SAMPLES_PER_CYCLE {
        sent += 1;
        match tcp_ping(target, timeout_dur).await {
            Ok(rtt) => {
                received += 1;
                rtts.push(rtt);
            }
            Err(err) => last_error = err,
        }
    }
    let loss_percent = if sent > 0 {
        f64::from(sent - received) / f64::from(sent) * 100.0
    } else {
        0.0
    };
    let latency_ms = if rtts.is_empty() {
        0.0
    } else {
        rtts.iter().sum::<f64>() / rtts.len() as f64
    };
    pb::ProbeObservation {
        target_id: target.id.clone(),
        kind: target.kind.clone(),
        check_id: target.check_id.clone(),
        cluster_id: target.cluster_id.clone(),
        provider_id: provider.id.clone(),
        host: target.host.clone(),
        port: u32::from(target.port),
        ip_family: target.ip_family.clone(),
        region: target.region.clone(),
        city: target.city.clone(),
        carrier: target.carrier.clone(),
        tags: target.tags.clone(),
        success: received > 0,
        latency_ms,
        jitter_ms: mean_jitter(&rtts),
        loss_percent,
        sent,
        received,
        error: last_error,
        event_time: event_time_now(),
    }
}

/// Each CIDR sample chooses one usable address uniformly without expanding
/// the network. IPv4 network/broadcast addresses are excluded below /31;
/// IPv6 has no broadcast address. The full IPv6 /0 is handled without overflow.
fn cidr_sample(host: &str, random: u128) -> Result<IpAddr, String> {
    let net: ipnet::IpNet = host.parse().map_err(|e| format!("CIDR: {e}"))?;
    Ok(match net {
        ipnet::IpNet::V4(net) => {
            let count = 1u64 << (32 - net.prefix_len());
            let (offset, count) = if net.prefix_len() < 31 {
                (1, count - 2)
            } else {
                (0, count)
            };
            IpAddr::V4(Ipv4Addr::from(
                u32::from(net.network()) + offset + (random % u128::from(count)) as u32,
            ))
        }
        ipnet::IpNet::V6(net) => {
            let bits = 128 - net.prefix_len();
            let offset = if bits == 128 {
                random
            } else {
                random % (1u128 << bits)
            };
            IpAddr::V6(Ipv6Addr::from(u128::from(net.network()) + offset))
        }
    })
}

fn matches_family(addr: IpAddr, family: &str) -> bool {
    match family {
        "v4" => addr.is_ipv4(),
        "v6" => addr.is_ipv6(),
        _ => false,
    }
}

async fn tcp_ping(target: &ProbeTargetPlan, timeout_dur: Duration) -> Result<f64, String> {
    let start = Instant::now();
    let addrs = match target.target_type.as_str() {
        "cidr" => {
            let ip = cidr_sample(&target.host, rand::random())?;
            vec![std::net::SocketAddr::new(ip, target.port)]
        }
        "host" => match timeout(
            timeout_dur,
            tokio::net::lookup_host((target.host.as_str(), target.port)),
        )
        .await
        {
            Ok(Ok(iter)) => iter.collect::<Vec<_>>(),
            Ok(Err(err)) => return Err(format!("resolve: {err}")),
            Err(_) => return Err("resolve timeout".into()),
        },
        _ => return Err("invalid target type".into()),
    };
    let addrs: Vec<_> = addrs
        .into_iter()
        .filter(|a| matches_family(a.ip(), &target.ip_family))
        .collect();
    if addrs.is_empty() {
        return Err("no address matching IP family".into());
    }
    let remaining = timeout_dur.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err("timeout".into());
    }
    // RTT starts after resolution: DNS lookup is not TCP connection latency.
    let connect_start = Instant::now();
    match timeout(remaining, TcpStream::connect(&addrs[..])).await {
        Ok(Ok(stream)) => {
            let rtt_ms = connect_start.elapsed().as_secs_f64() * 1000.0;
            drop(stream);
            Ok(rtt_ms)
        }
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("timeout".into()),
    }
}

/// mean_jitter is the mean absolute difference between consecutive RTTs; zero
/// when fewer than two samples succeeded.
fn mean_jitter(rtts: &[f64]) -> f64 {
    if rtts.len() < 2 {
        return 0.0;
    }
    let sum: f64 = rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    sum / (rtts.len() - 1) as f64
}

fn clamp_interval(seconds: u32) -> u32 {
    if seconds == 0 {
        return DEFAULT_INTERVAL_SECS;
    }
    seconds.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

fn clamp_timeout(ms: u32) -> u32 {
    if ms == 0 {
        return DEFAULT_TIMEOUT_MS;
    }
    ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS)
}

/// event_time_now formats the current (time-offset-adjusted) UTC instant in the
/// `2006-01-02 15:04:05.000` layout the ingest decoder requires.
fn event_time_now() -> String {
    let ms = crate::utils::time::now_timestamp_millis();
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_sampling_is_bounded_and_excludes_v4_network_and_broadcast() {
        for seed in [0, 1, 2, u128::MAX] {
            let ip = cidr_sample("192.0.2.0/30", seed).unwrap();
            assert!(
                ip == "192.0.2.1".parse::<IpAddr>().unwrap()
                    || ip == "192.0.2.2".parse::<IpAddr>().unwrap()
            );
            assert!(
                "2001:db8::/32"
                    .parse::<ipnet::IpNet>()
                    .unwrap()
                    .contains(&cidr_sample("2001:db8::/32", seed).unwrap())
            );
        }
        assert_eq!(
            cidr_sample("192.0.2.7/32", u128::MAX).unwrap().to_string(),
            "192.0.2.7"
        );
        assert_eq!(
            cidr_sample("192.0.2.0/31", 1).unwrap().to_string(),
            "192.0.2.1"
        );
        assert_eq!(
            cidr_sample("::/0", u128::MAX).unwrap(),
            IpAddr::V6(Ipv6Addr::from(u128::MAX))
        );
        assert_eq!(
            cidr_sample("2001:db8::1/128", 12).unwrap().to_string(),
            "2001:db8::1"
        );
        assert!(cidr_sample("invalid", 0).is_err());
    }

    #[tokio::test]
    async fn tcp_ping_enforces_family_and_cidr_is_connected_without_dns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = ProbeTargetPlan {
            kind: "tcp".into(),
            check_id: "probe:tcp:0123456789abcdef".into(),
            cluster_id: "ec_test".into(),
            target_type: "cidr".into(),
            host: "127.0.0.1/32".into(),
            port: listener.local_addr().unwrap().port(),
            ip_family: "v4".into(),
            ..Default::default()
        };
        assert!(tcp_ping(&target, Duration::from_secs(1)).await.is_ok());
        let _connected = listener.accept().await.unwrap();
        drop(listener);
        let observation = probe_once(
            &ProbeProviderPlan::default(),
            &target,
            Duration::from_secs(1),
        )
        .await;
        assert!(!observation.success);
        assert_eq!(observation.sent, SAMPLES_PER_CYCLE);
        assert_eq!(observation.received, 0);
        assert_eq!(observation.loss_percent, 100.0);
        assert!(!observation.error.is_empty());
        let wrong_family = ProbeTargetPlan {
            ip_family: "v6".into(),
            ..target
        };
        assert_eq!(
            tcp_ping(&wrong_family, Duration::from_secs(1))
                .await
                .unwrap_err(),
            "no address matching IP family"
        );
        assert!(matches_family("::1".parse().unwrap(), "v6"));
        assert!(!matches_family("::1".parse().unwrap(), "v4"));
    }

    #[tokio::test]
    async fn identical_plan_does_not_restart_generation_and_removal_stops_tasks() {
        let plan = ProbePlan {
            enabled: true,
            providers: vec![ProbeProviderPlan {
                targets: vec![ProbeTargetPlan {
                    kind: "tcp".into(),
                    check_id: "probe:tcp:feedfacefeedface".into(),
                    cluster_id: "ec_test".into(),
                    target_type: "host".into(),
                    host: "127.0.0.1".into(),
                    port: 9,
                    ip_family: "v4".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        apply_plan(Some(plan.clone()));
        let generation = GENERATION.load(Ordering::SeqCst);
        assert_eq!(TASKS.lock().len(), 1);
        apply_plan(Some(plan));
        assert_eq!(GENERATION.load(Ordering::SeqCst), generation);
        apply_plan(None);
        assert!(TASKS.lock().is_empty());
        assert!(APPLIED_PLAN.lock().is_none());
        assert!(GENERATION.load(Ordering::SeqCst) > generation);
    }

    #[test]
    fn probe_wire_contract_preserves_target_type_and_family() {
        let target: ProbeTargetPlan = serde_json::from_str(
            r#"{"kind":"tcp","checkId":"probe:tcp:0123456789abcdef","clusterId":"ec_test","targetType":"cidr","host":"2001:db8::/32","port":443,"ipFamily":"v6"}"#,
        )
        .unwrap();
        assert_eq!(target.target_type, "cidr");
        assert_eq!(target.ip_family, "v6");
        assert_eq!(target.check_id, "probe:tcp:0123456789abcdef");
        assert_eq!(target.cluster_id, "ec_test");
        assert_eq!(serde_json::to_value(target).unwrap()["targetType"], "cidr");
    }

    #[test]
    fn plan_identity_and_kind_survive_the_wire() {
        let plan: ProbePlan = serde_json::from_str(
            r#"{"enabled":true,"providers":[{"id":"pp_quality","kind":"custom","intervalSeconds":60,"timeoutMs":3000,"targets":[{"id":"pt_q","kind":"tcp","checkId":"probe:tcp:0123456789abcdef","clusterId":"ec_q","targetType":"host","host":"203.0.113.7","port":443,"ipFamily":"v4"}]}]}"#,
        )
        .unwrap();
        let target = &plan.providers[0].targets[0];
        assert_eq!(target.kind, "tcp");
        assert_eq!(target.check_id, "probe:tcp:0123456789abcdef");
        assert_eq!(target.cluster_id, "ec_q");
        let observation = pb::ProbeObservation {
            kind: target.kind.clone(),
            check_id: target.check_id.clone(),
            cluster_id: target.cluster_id.clone(),
            ..Default::default()
        };
        let encoded = prost::Message::encode_to_vec(&observation);
        let decoded = <pb::ProbeObservation as prost::Message>::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.check_id, "probe:tcp:0123456789abcdef");
        assert_eq!(decoded.cluster_id, "ec_q");
        assert_eq!(decoded.kind, "tcp");
    }

    #[test]
    fn jitter_is_zero_below_two_samples() {
        assert_eq!(mean_jitter(&[]), 0.0);
        assert_eq!(mean_jitter(&[5.0]), 0.0);
    }

    #[test]
    fn jitter_is_mean_absolute_difference() {
        assert!((mean_jitter(&[10.0, 14.0, 11.0]) - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn clamps_apply_defaults_and_bounds() {
        assert_eq!(clamp_interval(0), DEFAULT_INTERVAL_SECS);
        assert_eq!(clamp_interval(1), MIN_INTERVAL_SECS);
        assert_eq!(clamp_interval(1_000_000), MAX_INTERVAL_SECS);
        assert_eq!(clamp_timeout(0), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout(1), MIN_TIMEOUT_MS);
        assert_eq!(clamp_timeout(1_000_000), MAX_TIMEOUT_MS);
    }

    #[test]
    fn event_time_matches_ingest_layout() {
        let s = event_time_now();
        // 2006-01-02 15:04:05.000 → 23 chars, digits and separators only.
        assert_eq!(s.len(), 23);
        assert!(chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.3f").is_ok());
    }
}
