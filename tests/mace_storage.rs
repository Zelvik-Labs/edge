use zelvik_edge::metrics::storage::{
    MaceMemoryTier, MetricStorage, NODE_PERIOD_BYTES, SERVER_PERIOD_BYTES,
    apply_node_period_deltas, apply_server_period_update, fold_counter_deltas,
    mace_memory_observation, mace_memory_tier_for_stable_capacity,
};
use zelvik_edge::rpc::metrics::ServerMetricUpdate;

fn sample_update() -> ServerMetricUpdate {
    ServerMetricUpdate {
        server_id: 7,
        user_id: 0,
        user_plan_id: 0,
        plan_id: 0,
        total_requests: 10,
        bytes_sent: 100,
        bytes_received: 20,
        cached_bytes: 4,
        count_cached_requests: 1,
        count_attack_requests: 0,
        attack_bytes: 0,
        active_connections: 3,
        count_websocket_connections: 0,
        count_ips: 2,
    }
}

fn u64_be(buf: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&buf[offset..offset + 8]);
    u64::from_be_bytes(raw)
}

#[test]
fn fold_counter_deltas_coalesces_and_skips_zero() {
    let folded = fold_counter_deltas(vec![
        ("a".to_string(), 10),
        ("a".to_string(), 5),
        ("b".to_string(), 0),
        ("c".to_string(), 3),
    ]);
    let mut map: std::collections::HashMap<_, _> = folded.into_iter().collect();
    assert_eq!(map.remove("a"), Some(15));
    assert_eq!(map.remove("c"), Some(3));
    assert!(map.is_empty());
}

#[test]
fn packed_period_records_merge_in_memory() {
    let first = apply_server_period_update(None, &sample_update());
    assert_eq!(first.len(), SERVER_PERIOD_BYTES);
    assert_eq!(u64_be(&first, 0), 10);
    assert_eq!(u64_be(&first, 8), 100);

    let second = apply_server_period_update(Some(&first), &sample_update());
    assert_eq!(u64_be(&second, 0), 20);
    assert_eq!(u64_be(&second, 8), 200);
    assert_eq!(&second[56..64], &3i64.to_be_bytes());

    let node = apply_node_period_deltas(None, 50, 25);
    assert_eq!(node.len(), NODE_PERIOD_BYTES);
    let node = apply_node_period_deltas(Some(&node), 10, 5);
    assert_eq!(u64_be(&node, 0), 60);
    assert_eq!(u64_be(&node, 8), 30);
}

#[test]
fn metric_storage_increment_and_server_batch_round_trip() {
    let dir =
        std::env::temp_dir().join(format!("zelvik-edge-mace-opt-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&dir);
    let storage = MetricStorage::open(&dir).expect("open mace storage");

    storage
        .increment_batch(vec![
            ("counter_a".to_string(), 10),
            ("counter_a".to_string(), 5),
            ("counter_b".to_string(), 0),
        ])
        .expect("persist counter batch");
    assert_eq!(storage.get_value("counter_a"), 15);
    assert_eq!(storage.get_value("counter_b"), 0);

    storage
        .record_server_batch(1_700_000_000, vec![sample_update()], 50, 25)
        .expect("persist first server batch");
    storage
        .record_server_batch(1_700_000_000, vec![sample_update()], 10, 5)
        .expect("persist second server batch");
    drop(storage);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn old_metadata_format_is_rejected_without_rewriting_the_manifest() {
    let path = std::env::temp_dir().join(format!("zelvik-mace-v1-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(path.join("log")).unwrap();
    // Two checksum-valid format-1 meta headers from the stopped development
    // database. Payload references are never followed for an unsupported format.
    let mut manifest = vec![0u8; 8192];
    for (offset, header) in [
        (0, "6565727462776f633d0100000000000001000000330000009100000002000000050000003c3a4833"),
        (4096, "6565727462776f633e01000000000000010000001600000091000000030000000400000065f6a75c"),
    ] {
        let header = hex::decode(header).unwrap();
        manifest[offset..offset + header.len()].copy_from_slice(&header);
    }
    let manifest_path = path.join("log/manifest");
    std::fs::write(&manifest_path, &manifest).unwrap();
    let report = mace::check_database(&mace::Options::new(&path));
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.category == mace::CheckCategory::UnsupportedMetadataFormat),
        "{report:?}"
    );
    let error = match MetricStorage::open(&path) {
        Ok(_) => panic!("format-1 storage must not open"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("Corruption"), "{error}");
    assert_eq!(std::fs::read(manifest_path).unwrap(), manifest);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn mace_crash_child() {
    let Ok(path) = std::env::var("ZELVIK_MACE_CRASH_FIXTURE") else {
        return;
    };
    let storage = MetricStorage::open(path).expect("open child storage");
    assert!(storage.put_json("CRASH_COMMITTED", &42u64));
    // Process exit deliberately bypasses engine and transaction destructors.
    std::process::exit(0);
}

#[test]
fn committed_wal_survives_exit_without_engine_shutdown() {
    let path = std::env::temp_dir().join(format!("zelvik-mace-crash-{}", uuid::Uuid::new_v4()));
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mace_crash_child", "--nocapture"])
        .env("ZELVIK_MACE_CRASH_FIXTURE", &path)
        .status()
        .unwrap();
    assert!(status.success());
    let storage = MetricStorage::open(&path).expect("recover committed WAL");
    assert_eq!(storage.get_json::<u64>("CRASH_COMMITTED"), Some(42));
    drop(storage);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn mace_tier_uses_stable_capacity_boundaries() {
    assert_eq!(
        mace_memory_tier_for_stable_capacity(512 * 1024 * 1024),
        MaceMemoryTier::MiB512
    );
    assert_eq!(
        mace_memory_tier_for_stable_capacity(1024 * 1024 * 1024),
        MaceMemoryTier::GiB1
    );
    assert_eq!(
        mace_memory_tier_for_stable_capacity(2 * 1024 * 1024 * 1024),
        MaceMemoryTier::GiB2
    );
}

#[test]
fn mace_memory_observation_labels_capacity_semantics() {
    let observation = mace_memory_observation();
    assert_eq!(
        observation.wal_buffer_capacity_bytes,
        observation
            .wal_buffer_bytes_per_group
            .saturating_mul(observation.concurrent_write as u64)
    );
    assert!(observation.wal_file_size_bytes >= observation.wal_buffer_bytes_per_group);
    assert!(observation.bucket_cache_capacity_bytes > 0);
    assert!(observation.bucket_pool_capacity_bytes > 0);
    assert!(observation.bucket_checkpoint_size_bytes > 0);
}
