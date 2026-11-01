//! Issue 34 full-Edge lane: the *actual* binary must complete production
//! startup against a controlled control plane.
//!
//! This test is opt-in because it spawns the real `Zelvik-Edge`, binds a TLS
//! listener, and shells out to `curl`/`openssl`. It is not a component bench:
//! a pass means the binary read `configs/api_node.yaml`, registered on the
//! control channel, fetched and HMAC-verified a NodeControl snapshot, applied
//! it, served HTTP/1.1 and HTTP/2 through the real TLS listener, proxied the
//! exact payload to the fixture origin, reported structured health, and
//! observed telemetry `UNIMPLEMENTED` as a terminal rejection (not an ignored
//! fallback).
//!
//! Run with:
//! `ISSUE34_FULL_EDGE=1 cargo test --test issue34_full_edge -- --nocapture`

#[path = "../examples/issue34-controller.rs"]
mod controller;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_edge_completes_production_startup() -> anyhow::Result<()> {
    if std::env::var("ISSUE34_FULL_EDGE").as_deref() != Ok("1") {
        eprintln!(
            "skipping the full-Edge lane; set ISSUE34_FULL_EDGE=1 to spawn the real binary"
        );
        return Ok(());
    }

    let home = std::env::temp_dir().join(format!(
        "issue34-full-edge-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&home);

    // Drive the same CLI the soak wrapper uses, so the test also pins the
    // structured fixture contract (spawn/smoke/report/cleanup).
    let options = controller::Options::parse([
        "--home".to_string(),
        home.display().to_string(),
        "--edge-binary".to_string(),
        env!("CARGO_BIN_EXE_Zelvik-Edge").to_string(),
        "--spawn-edge".to_string(),
        "--smoke".to_string(),
        "--report-file".to_string(),
        home.join("report.json").display().to_string(),
        "--cleanup-home".to_string(),
    ])?;

    let report = controller::run(options).await?;
    let rendered = serde_json::to_string_pretty(&report)?;
    assert_eq!(
        report["status"].as_str(),
        Some("pass"),
        "full-Edge fixture did not pass:\n{rendered}"
    );
    assert_eq!(report["node_health_observed"].as_bool(), Some(true));
    assert!(
        report["counters"]["registerInstance"].as_u64().unwrap_or(0) >= 1,
        "the Edge must activate on the control channel"
    );
    assert!(
        report["counters"]["registerNode"].as_u64().unwrap_or(0) >= 1,
        "the Edge must register with NodeControl"
    );
    assert!(
        report["counters"]["getDesiredRelease"].as_u64().unwrap_or(0) >= 1,
        "the Edge must fetch the signed snapshot"
    );
    assert!(
        report["counters"]["reportNodeHealth"].as_u64().unwrap_or(0) >= 1,
        "the Edge must report structured health"
    );

    let checks = report["checks"]["checks"]
        .as_array()
        .expect("fixture smoke checks");
    let http1 = checks
        .iter()
        .find(|check| check["name"] == "http1-post")
        .expect("HTTP/1.1 check");
    assert_eq!(http1["ok"].as_bool(), Some(true));
    assert_eq!(http1["http_version"].as_str(), Some("1.1"));
    assert_eq!(http1["http_code"].as_u64(), Some(200));
    assert!(http1["payload_bytes"].as_u64().unwrap_or(0) > 0);
    let http2 = checks
        .iter()
        .find(|check| check["name"] == "http2-post")
        .expect("HTTP/2 check");
    assert_eq!(http2["ok"].as_bool(), Some(true));
    assert_eq!(http2["http_version"].as_str(), Some("2"));
    assert_eq!(http2["http_code"].as_u64(), Some(200));
    assert_eq!(
        http1["payload_sha256"], http2["payload_sha256"],
        "both protocols must carry the exact origin payload"
    );
    let alpn = checks
        .iter()
        .find(|check| check["name"] == "openssl-alpn")
        .or_else(|| checks.iter().find(|check| check["alpn"] == "h2"))
        .expect("ALPN/SNI check");
    assert_eq!(alpn["alpn"].as_str(), Some("h2"));
    assert_eq!(alpn["peer_verified"].as_bool(), Some(true));
    let telemetry = checks
        .iter()
        .find(|check| check["name"] == "telemetry-reject")
        .expect("telemetry rejection check");
    assert_eq!(telemetry["result"]["grpc_code"].as_str(), Some("UNIMPLEMENTED"));
    assert_eq!(telemetry["result"]["code_number"].as_u64(), Some(12));
    assert_eq!(telemetry["result"]["acknowledged"].as_bool(), Some(false));
    let retry = checks
        .iter()
        .find(|check| check["name"] == "telemetry-retryable-recovery")
        .expect("telemetry retry/recovery check");
    assert_eq!(retry["ok"].as_bool(), Some(true));
    assert!(retry["pending_before"].as_u64().unwrap_or(0) >= 1);
    assert!(retry["max_batch_bytes"].as_u64().unwrap_or(0) > 0);
    assert_eq!(retry["pending_after"].as_u64(), Some(0));
    assert_eq!(retry["core_pending_after"].as_u64(), Some(0));
    assert_eq!(retry["detail_pending_after"].as_u64(), Some(0));
    assert!(retry["acked_sequence"].as_u64().unwrap_or(0) >= retry["max_sequence_before"].as_u64().unwrap_or(u64::MAX));
    assert_eq!(retry["recovered"].as_bool(), Some(true));
    assert_eq!(report["telemetry"]["served"].as_bool(), Some(false));
    assert_eq!(report["telemetry"]["mode"].as_str(), Some("unimplemented"));
    assert_eq!(report["pid_at_exit"], report["edge_pid"]);

    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}
