//! Active network-quality probing (tcping, HTTP and TLS).
//!
//! The control plane pushes a per-node plan through `nodeJSON.runtime.probe`
//! (see [`crate::config_models::ProbePlan`]). Each target is probed on its
//! provider's cadence; every cycle pushes one observation into a shared buffer
//! that a per-second flusher folds into a single `probe_observation` batch, so
//! N targets cost one upload per second instead of N. There is no local target
//! list — probing follows the pushed plan only.
//!
//! Which kinds appear is a property of the source, not of this executor: the
//! built-in zstaticcdn catalog is L4-only and its rows are `kind=tcp`, while a
//! custom source may dispatch `http`/`https` or `tls` here.
//!
//! A plan is applied atomically: bumping the generation retires the previous
//! target tasks, so a removed target stops probing immediately.

use crate::config_models::{ProbePlan, ProbeProviderPlan, ProbeTargetPlan};
use crate::pb;
use rustls::pki_types::ServerName;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};
use tokio_rustls::TlsConnector;
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

/// Probe kinds this executor can run. `dns` belongs to the probe node's service
/// health chain and is deliberately absent.
const KIND_TCP: &str = "tcp";
const KIND_HTTP: &str = "http";
const KIND_TLS: &str = "tls";

/// Shared HTTP client for `http` targets: redirects are not followed, because a
/// 3xx is itself a successful probe result rather than a hop to chase.
static HTTP_CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("http client: {err}"))
});

/// Shared TLS client config for `tls` targets: native roots and full chain +
/// hostname verification, so an expired, self-signed or wrong-host certificate
/// fails the handshake instead of counting as reachable.
static TLS_CONFIG: LazyLock<Result<Arc<rustls::ClientConfig>, String>> = LazyLock::new(|| {
    let provider = crate::tls_crypto::default_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        roots
            .add(cert)
            .map_err(|err| format!("tls root store: {err}"))?;
    }
    if !native.errors.is_empty() {
        warn!(
            errors = native.errors.len(),
            "probe TLS: ignored native root store errors"
        );
    }
    let config = rustls::ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
        .map_err(|err| format!("tls config: {err}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
});

/// 观测缓冲：每个目标的每个周期只往这里推一条，由 flusher 合并成一批上报。
///
/// 一条观测一个批次会让 detail 通道的串行上传成为吞吐上限（实测恒为
/// 60 批/分钟），目标一多就必然积压、滞后单调增长。合并后一次上传承载
/// 该窗口内的全部观测，吞吐随目标数线性扩展，而通道压力恒定。
static OUTBOX: LazyLock<parking_lot::Mutex<Vec<pb::ProbeObservation>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));
/// 合并窗口。与 flusher 的 tick 一致，避免同一观测跨窗口。
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

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
    // 计划生效即启动合并上报任务；清空计划时它随 generation 一起退出。
    tasks.push(tokio::spawn(flush_observations(generation)));
    let mut spawned = 0usize;
    let mut trimmed = 0usize;
    for provider in &plan.providers {
        // One shared provider per target task: the cadence and id are constant
        // across a provider's targets, so the plan is not cloned per target.
        let provider = Arc::new(provider.clone());
        for target in &provider.targets {
            // Identity and family come from the control plane plan: an Edge row
            // must carry the exact check/cluster the quality view authorizes.
            // The kind decides which executor runs, not whether the target is
            // admitted: zstaticcdn's L4-only nature is expressed by its catalog
            // rows being tcp, not by a rule here.
            if !matches!(target.kind.as_str(), KIND_TCP | KIND_HTTP | KIND_TLS)
                || target.check_id.is_empty()
                || target.cluster_id.is_empty()
                || target.host.trim().is_empty()
                || target.port == 0
            {
                warn!(target = %target.id, kind = %target.kind, "network quality target rejected: incomplete plan identity or unsupported kind");
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

/// flush_observations 把缓冲里的观测每秒合并成一个批次入队一次。
/// 空窗口不发包；入队失败时把观测放回缓冲，下一个窗口重试。
async fn flush_observations(generation: u64) {
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        if GENERATION.load(Ordering::SeqCst) != generation {
            break;
        }
        let Some(batch) = drain_outbox() else {
            continue;
        };
        let count = batch.observations.len();
        let drained = batch.observations.clone();
        if let Err(err) = crate::telemetry::enqueue("probe_observation", &batch, 0, false).await {
            warn!(observation_count = count, error = %err, "probe observation enqueue failed; retained");
            OUTBOX.lock().extend(drained);
        }
    }
}

/// drain_outbox 把当前窗口的观测合并成一个批次；空窗口返回 None。
///
/// 合并是吞吐的关键：93 个目标 × 15 秒 = 每分钟 372 条，一条一批会把
/// detail 通道压到 60 批/分钟的上限，从这里开始就必然积压。
fn drain_outbox() -> Option<pb::ProbeObservationBatch> {
    let drained = std::mem::take(&mut *OUTBOX.lock());
    if drained.is_empty() {
        return None;
    }
    Some(pb::ProbeObservationBatch { observations: drained })
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
        OUTBOX.lock().push(observation);
    }
}

/// probe_once runs one cycle of samples for the target's kind and folds them
/// into a single observation: latency is the mean of successful RTTs, jitter
/// the mean absolute difference between consecutive RTTs, loss the failed
/// fraction.
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
        match probe_sample(target, timeout_dur).await {
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

/// probe_sample dispatches one sample to the executor the plan kind names.
async fn probe_sample(target: &ProbeTargetPlan, timeout_dur: Duration) -> Result<f64, String> {
    match target.kind.as_str() {
        KIND_TCP => tcp_ping(target, timeout_dur).await,
        KIND_HTTP => http_probe(target, timeout_dur).await,
        KIND_TLS => tls_probe(target, timeout_dur).await,
        other => Err(format!("unsupported probe kind {other}")),
    }
}

/// http_probe_url resolves the request URL from the plan. A `url` target already
/// carries its scheme; a `host` target is an HTTP endpoint on its plan port.
fn http_probe_url(target: &ProbeTargetPlan) -> Result<String, String> {
    match target.target_type.as_str() {
        "url" => Ok(target.host.clone()),
        "host" => {
            // An IPv6 literal needs brackets inside a URL authority.
            let host = if target.host.starts_with('[') {
                target.host.clone()
            } else if let Ok(addr) = target.host.parse::<Ipv6Addr>() {
                format!("[{addr}]")
            } else {
                target.host.clone()
            };
            Ok(format!("http://{host}:{}/", target.port))
        }
        other => Err(format!(
            "http probes require a host or url target, got {other}"
        )),
    }
}

/// http_probe issues one request and treats 2xx/3xx as success. The status code
/// is reported in `error` when the response is a failure: the observation wire
/// format has no status column, and latency is the request's elapsed time.
///
/// Unlike `tcp`/`tls`, address selection follows the HTTP client's own
/// resolution, so `ipFamily` is a preference here rather than a hard filter.
async fn http_probe(target: &ProbeTargetPlan, timeout_dur: Duration) -> Result<f64, String> {
    let url = http_probe_url(target)?;
    let url = reqwest::Url::parse(&url).map_err(|err| format!("url: {err}"))?;
    let client = HTTP_CLIENT.as_ref().map_err(|err| err.clone())?;
    let start = Instant::now();
    let response = timeout(timeout_dur, client.get(url).send())
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|err| format!("http: {err}"))?;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let status = response.status();
    if status.is_success() || status.is_redirection() {
        return Ok(elapsed_ms);
    }
    Err(format!("http status {}", status.as_u16()))
}

/// tls_endpoint extracts the hostname and port a TLS handshake must target. A
/// url target keeps its explicit port, otherwise the plan port applies.
fn tls_endpoint(target: &ProbeTargetPlan) -> Result<(String, u16), String> {
    match target.target_type.as_str() {
        "url" => {
            let url =
                reqwest::Url::parse(&target.host).map_err(|err| format!("url: {err}"))?;
            let host = url
                .host_str()
                .ok_or_else(|| "url has no host".to_string())?
                .to_string();
            Ok((host, url.port().unwrap_or(target.port)))
        }
        "host" => Ok((target.host.clone(), target.port)),
        other => Err(format!("tls probes require a host or url target, got {other}")),
    }
}

/// tls_probe completes a verified TLS handshake and returns its duration. A
/// certificate rustls cannot verify fails the handshake, so an expired or
/// self-signed endpoint is a failed probe rather than a reachable one.
async fn tls_probe(target: &ProbeTargetPlan, timeout_dur: Duration) -> Result<f64, String> {
    let (host, port) = tls_endpoint(target)?;
    let server_name = ServerName::try_from(host.clone()).map_err(|err| format!("sni: {err}"))?;
    let config = TLS_CONFIG.as_ref().map_err(|err| err.clone())?;
    let start = Instant::now();
    let addrs = lookup_addrs(&host, port, timeout_dur).await?;
    let remaining = timeout_dur.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err("timeout".into());
    }
    let stream = timeout(remaining, TcpStream::connect(&addrs[..]))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|err| err.to_string())?;
    let remaining = timeout_dur.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err("timeout".into());
    }
    // RTT covers the handshake only: resolution and TCP connect are budgeted
    // separately so a slow resolver cannot look like a slow TLS endpoint.
    let handshake_start = Instant::now();
    let stream = timeout(
        remaining,
        TlsConnector::from(Arc::clone(config)).connect(server_name, stream),
    )
    .await
    .map_err(|_| "timeout".to_string())?
    .map_err(|err| format!("tls: {err}"))?;
    let rtt_ms = handshake_start.elapsed().as_secs_f64() * 1000.0;
    drop(stream);
    Ok(rtt_ms)
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

/// lookup_addrs resolves a host to socket addresses within the probe timeout.
async fn lookup_addrs(host: &str, port: u16, timeout_dur: Duration) -> Result<Vec<SocketAddr>, String> {
    match timeout(timeout_dur, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(iter)) => Ok(iter.collect::<Vec<_>>()),
        Ok(Err(err)) => Err(format!("resolve: {err}")),
        Err(_) => Err("resolve timeout".into()),
    }
}

async fn tcp_ping(target: &ProbeTargetPlan, timeout_dur: Duration) -> Result<f64, String> {
    let start = Instant::now();
    let addrs = match target.target_type.as_str() {
        "cidr" => {
            let ip = cidr_sample(&target.host, rand::random())?;
            vec![std::net::SocketAddr::new(ip, target.port)]
        }
        "host" => lookup_addrs(&target.host, target.port, timeout_dur).await?,
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

    /// 合并上报的核心契约：同一窗口内的多条观测必须进入同一个批次，
    /// 否则 detail 通道的「一批一次 RTT」会重新变成吞吐上限。
    #[test]
    fn drain_outbox_merges_window_observations_into_one_batch() {
        OUTBOX.lock().clear();
        assert!(drain_outbox().is_none(), "空窗口不应产生批次");
        for index in 0..93u32 {
            OUTBOX.lock().push(pb::ProbeObservation {
                target_id: format!("pt_{index}"),
                ..Default::default()
            });
        }
        let batch = drain_outbox().expect("窗口内有观测时必须产出批次");
        assert_eq!(batch.observations.len(), 93);
        assert_eq!(batch.observations[7].target_id, "pt_7");
        assert!(drain_outbox().is_none(), "取走之后缓冲必须清空，避免重复上报");
    }

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
        let _guard = PLAN_TEST_LOCK.lock().unwrap();
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

    // ---- L7 (http/tls) quality probe tests (begin) ----

    /// apply_plan mutates process-wide statics, so plan tests must not overlap.
    static PLAN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// l7_target builds one plan target for a kind under test.
    fn l7_target(kind: &str, target_type: &str, host: &str, port: u16) -> ProbeTargetPlan {
        ProbeTargetPlan {
            id: format!("pt_{kind}"),
            kind: kind.into(),
            check_id: format!("probe:{kind}:0123456789abcdef"),
            cluster_id: "ec_l7".into(),
            target_type: target_type.into(),
            host: host.into(),
            port,
            ip_family: "v4".into(),
            ..Default::default()
        }
    }

    /// serve_http answers every request with one fixed response, then closes.
    async fn serve_http(response: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf).await;
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        port
    }

    /// serve_tcp accepts connections and either drops them at once (a failed
    /// TLS handshake) or holds them open forever (a timeout).
    async fn serve_tcp(hold: bool) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if hold {
                    held.push(stream);
                } else {
                    drop(stream);
                }
            }
        });
        port
    }

    async fn sample(kind: &str, port: u16, timeout_dur: Duration) -> pb::ProbeObservation {
        probe_once(
            &ProbeProviderPlan::default(),
            &l7_target(kind, "host", "127.0.0.1", port),
            timeout_dur,
        )
        .await
    }

    /// 2xx and 3xx are success; anything else is a failure that reports the
    /// status code, and the elapsed request time is kept as the latency.
    #[tokio::test]
    async fn http_probe_succeeds_on_2xx_and_3xx_and_fails_on_other_status() {
        let ok = serve_http("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let observation = sample("http", ok, Duration::from_secs(5)).await;
        assert!(observation.success, "{}", observation.error);
        assert_eq!(observation.kind, "http");
        assert_eq!(observation.received, SAMPLES_PER_CYCLE);
        assert!(observation.latency_ms > 0.0);
        assert!(observation.error.is_empty());

        let redirect = serve_http(
            "HTTP/1.1 302 Found\r\nLocation: https://elsewhere.example/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let observation = sample("http", redirect, Duration::from_secs(5)).await;
        assert!(observation.success, "{}", observation.error);
        assert_eq!(observation.received, SAMPLES_PER_CYCLE);

        let missing = serve_http("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let observation = sample("http", missing, Duration::from_secs(5)).await;
        assert!(!observation.success);
        assert_eq!(observation.received, 0);
        assert_eq!(observation.loss_percent, 100.0);
        assert!(
            observation.error.contains("404"),
            "status code must be reported: {}",
            observation.error
        );

        let broken = serve_http("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let observation = sample("http", broken, Duration::from_secs(5)).await;
        assert!(!observation.success);
        assert!(
            observation.error.contains("500"),
            "status code must be reported: {}",
            observation.error
        );
    }

    /// A TLS endpoint that cannot complete a verified handshake is a failure.
    #[tokio::test]
    async fn tls_probe_fails_when_the_handshake_fails() {
        let port = serve_tcp(false).await;
        let observation = sample("tls", port, Duration::from_secs(5)).await;
        assert!(!observation.success);
        assert_eq!(observation.kind, "tls");
        assert_eq!(observation.received, 0);
        assert_eq!(observation.loss_percent, 100.0);
        assert!(
            !observation.error.is_empty(),
            "a failed handshake must carry a reason"
        );
    }

    /// A peer that never answers is a timeout for both L7 kinds, not a hang.
    #[tokio::test]
    async fn l7_probe_times_out_when_the_peer_never_answers() {
        for kind in ["http", "tls"] {
            let port = serve_tcp(true).await;
            let observation = sample(kind, port, Duration::from_millis(300)).await;
            assert!(!observation.success, "{kind} must not pass on a silent peer");
            assert_eq!(observation.received, 0, "{kind}");
            assert_eq!(observation.error, "timeout", "{kind}");
        }
    }

    /// The plan admits the kinds this executor runs and drops the rest, keeping
    /// the pushed check/cluster identity intact.
    #[tokio::test]
    async fn plan_dispatches_l7_kinds_and_drops_unknown_kinds() {
        let _guard = PLAN_TEST_LOCK.lock().unwrap();
        let plan = ProbePlan {
            enabled: true,
            providers: vec![ProbeProviderPlan {
                id: "pp_custom".into(),
                kind: "custom".into(),
                targets: vec![
                    l7_target("tcp", "host", "127.0.0.1", 9),
                    l7_target("http", "host", "127.0.0.1", 9),
                    l7_target("tls", "host", "127.0.0.1", 9),
                    l7_target("dns", "host", "127.0.0.1", 9),
                    ProbeTargetPlan {
                        kind: String::new(),
                        ..l7_target("tcp", "host", "127.0.0.1", 9)
                    },
                ],
                ..Default::default()
            }],
        };
        apply_plan(Some(plan));
        assert_eq!(
            TASKS.lock().len(),
            3,
            "tcp/http/tls must be dispatched, dns and an empty kind must not"
        );
        apply_plan(None);
        assert!(TASKS.lock().is_empty());
    }

    /// The observation an L7 target produces keeps the plan identity the
    /// authorizer pins, with the kind the plan named.
    #[tokio::test]
    async fn l7_observation_carries_plan_identity_and_kind() {
        let ok = serve_http("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        let target = l7_target("http", "host", "127.0.0.1", ok);
        let provider = ProbeProviderPlan {
            id: "pp_custom".into(),
            ..Default::default()
        };
        let observation = probe_once(&provider, &target, Duration::from_secs(5)).await;
        assert_eq!(observation.kind, "http");
        assert_eq!(observation.check_id, "probe:http:0123456789abcdef");
        assert_eq!(observation.cluster_id, "ec_l7");
        assert_eq!(observation.provider_id, "pp_custom");
        assert_eq!(observation.target_id, "pt_http");
        assert_eq!(observation.port, u32::from(ok));
        assert_eq!(observation.ip_family, "v4");
        assert_eq!(observation.event_time.len(), 23);
    }

    // ---- L7 (http/tls) quality probe tests (end) ----

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
