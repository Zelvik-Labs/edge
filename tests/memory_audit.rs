//! Linux process-level memory cycles. Run this executable in its own cgroup;
//! it records physical observations separately from logical structure bytes.
#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Instant;
use zelvik_edge::memory_governor::{AdmissionClass, MEMORY_GOVERNOR, MemoryPressureLevel};
use zelvik_edge::memory_reclaim::reclaim_for_level;
use zelvik_edge::metrics::aggregator::{AggregationKey, MetricAggregator};

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn key(cycle: usize, row: usize) -> AggregationKey {
    AggregationKey {
        category: Arc::from("http"),
        server_id: 1,
        country: Arc::from(""),
        country_id: 0,
        province: Arc::from(""),
        province_id: 0,
        city: Arc::from(format!("/audit/{cycle}/{row}/{}", "x".repeat(128))),
        city_id: 0,
        provider: Arc::from("Unknown"),
        browser: Arc::from(""),
        os: Arc::from(""),
        waf_group_id: 0,
        waf_action: Arc::from(""),
        provider_id: 0,
        browser_version: Arc::from(""),
        os_version: Arc::from(""),
    }
}

fn env_size(name: &str, default: usize) -> usize {
    std::env::var(name)
        .map(|value| value.parse().expect("positive audit setting"))
        .unwrap_or(default)
}

#[test]
fn repeated_aggregation_and_reclaim_converge_with_held_admissions() {
    let cycles = env_size("MEMORY_AUDIT_CYCLES", 8);
    let rows = env_size("MEMORY_AUDIT_ROWS", 30_000);
    assert!(cycles > 0 && rows > 0);
    let connections: Vec<_> = (0..16)
        .map(|_| {
            MEMORY_GOVERNOR
                .try_admit(AdmissionClass::HttpConnection)
                .expect("initial connections fit")
        })
        .collect();
    let aggregator = MetricAggregator::new();
    let baseline = MEMORY_GOVERNOR.snapshot(1);
    assert!(
        baseline.process_rss_bytes > 0,
        "Linux RSS must be observable"
    );
    let mut final_rss = Vec::new();
    let started = Instant::now();
    for cycle in 0..cycles {
        // Admission limits follow live RSS and host headroom. A later, smaller
        // limit stops growth; it does not evict already admitted rows before
        // flush. Check the cycle's admission envelope, then verify reclamation.
        let mut budget = MEMORY_GOVERNOR.metrics_aggregator_limits().1;
        for row in 0..rows {
            budget = budget.max(MEMORY_GOVERNOR.metrics_aggregator_limits().1);
            aggregator.record(key(cycle, row), 1024, 64, false);
            budget = budget.max(MEMORY_GOVERNOR.metrics_aggregator_limits().1);
            assert!(aggregator.approximate_bytes() <= budget, "aggregate byte budget exceeded");
        }
        let charged = aggregator.approximate_bytes();
        let before = MEMORY_GOVERNOR.snapshot(1);
        let drained = aggregator.flush();
        assert!(aggregator.data.is_empty());
        assert_eq!(aggregator.approximate_bytes(), 0);
        drop(drained);
        let reclaimed = reclaim_for_level(MemoryPressureLevel::Critical);
        MEMORY_GOVERNOR.account_view_fresh();
        let after = MEMORY_GOVERNOR.snapshot(1);
        assert!(after.process_rss_bytes > 0 && after.process_pss_bytes > 0);
        assert_eq!(
            after.estimated_http_connections,
            baseline.estimated_http_connections
        );
        assert!(
            MEMORY_GOVERNOR
                .try_admit(AdmissionClass::Http2Stream)
                .is_some(),
            "existing connection must still make request progress"
        );
        final_rss.push(after.process_rss_bytes);
        println!(
            "MEMORY_AUDIT cycle={cycle} rows={rows} charged={charged} budget={budget} rss_before={} rss_after={} pss={} anon={} pressure={} grantable={} borrowable={} reclaim_estimate={} elapsed_ms={}",
            before.process_rss_bytes,
            after.process_rss_bytes,
            after.process_pss_bytes,
            after.process_anon_rss_bytes,
            after.memory_pressure_level.as_str(),
            after.account_grantable_bytes,
            after.low_priority_borrowable_bytes,
            reclaimed.freed_bytes_estimate(),
            started.elapsed().as_millis(),
        );
    }
    // Repeated identical-sized cycles may warm allocator/container metadata;
    // their retained RSS must not grow by another full aggregator budget.
    let allowance = MEMORY_GOVERNOR.metrics_aggregator_limits().1.max(16 << 20);
    assert!(
        *final_rss.last().expect("cycles") <= final_rss[0].saturating_add(allowance),
        "retained RSS grew beyond one aggregate budget: {final_rss:?}"
    );
    drop(connections);
    assert_eq!(MEMORY_GOVERNOR.snapshot(1).estimated_http_connections, 0);
}
