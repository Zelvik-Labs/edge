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

/// apply_plan replaces the running probe set. `None` (or `enabled = false`)
/// stops every probe — the control plane removes the key when a node's cluster
/// has probing off or no bindings.
pub fn apply_plan(plan: Option<ProbePlan>) {
    let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let mut tasks = TASKS.lock();
    for handle in tasks.drain(..) {
        handle.abort();
    }
    let plan = match plan {
        Some(plan) if plan.enabled => plan,
        _ => {
            debug!("probe plan cleared");
            return;
        }
    };
    let mut spawned = 0usize;
    let mut trimmed = 0usize;
    for provider in plan.providers {
        // One shared provider per target task: the cadence and id are constant
        // across a provider's targets, so the plan is not cloned per target.
        let provider = Arc::new(provider);
        for target in &provider.targets {
            if target.host.trim().is_empty() || target.port == 0 {
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
        match tcp_ping(&target.host, target.port, timeout_dur).await {
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

/// tcp_ping resolves the host asynchronously (never blocking the runtime) then
/// times a TCP connect to the first usable address within the timeout.
async fn tcp_ping(host: &str, port: u16, timeout_dur: Duration) -> Result<f64, String> {
    let start = Instant::now();
    let addrs = match timeout(timeout_dur, tokio::net::lookup_host((host, port))).await {
        Ok(Ok(iter)) => iter.collect::<Vec<_>>(),
        Ok(Err(err)) => return Err(format!("resolve: {err}")),
        Err(_) => return Err("resolve timeout".into()),
    };
    if addrs.is_empty() {
        return Err("no address".into());
    }
    let remaining = timeout_dur.saturating_sub(start.elapsed());
    let budget = remaining.max(Duration::from_millis(1));
    match timeout(budget, TcpStream::connect(&addrs[..])).await {
        Ok(Ok(stream)) => {
            let rtt_ms = start.elapsed().as_secs_f64() * 1000.0;
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
