//! Real legacy-engine fixtures, including committed WAL without a clean close.
use std::path::Path;
use zelvik_edge::storage_migration::{ensure_ready, mark_ready, prepare, store_root};

fn legacy_options(path: &Path) -> legacy_mace::Options {
    let mut options = legacy_mace::Options::new(path);
    options.sync_on_write = true;
    options.lru_capacity = 8 << 20;
    options.wal_buffer_size = 1 << 20;
    options.wal_file_size = 4 << 20;
    options
}

fn seed(path: &Path) {
    let db = legacy_mace::Mace::new(legacy_options(path).validate().unwrap()).unwrap();
    for name in ["metrics", "queue", "empty"] {
        let bucket = db.new_bucket(name, legacy_mace::BucketOptions {
            cache_capacity: 8 << 20,
            pool_capacity: 8 << 20,
            checkpoint_size: 2 << 20,
            ..Default::default()
        }).unwrap();
        if name == "empty" { continue; }
        let txn = bucket.begin().unwrap();
        for (key, value) in [
            (b"counter".as_slice(), b"42".as_slice()),
            (b"state:producer", b"{\"cursor\":17}"),
            (b"ack:00000000000000000002", b"receipt"),
            (b"batch:00000000000000000003", b"unconfirmed payload"),
            (b"CTOMB_hash", b"tombstone"),
            (b"deleted", b"must not return"),
        ] { txn.put(key, value).unwrap(); }
        txn.commit().unwrap();
        let txn = bucket.begin().unwrap();
        txn.del(b"deleted").unwrap();
        txn.commit().unwrap();
    }
    db.sync().unwrap();
}

#[test]
fn legacy_wal_child() {
    let Some(path) = std::env::var_os("ZELVIK_LEGACY_MIGRATION_CHILD") else { return; };
    // exit() deliberately bypasses destructors; committed WAL must be replayed.
    let db = legacy_mace::Mace::new(legacy_options(Path::new(&path)).validate().unwrap()).unwrap();
    let bucket = db.new_bucket("queue", Default::default()).unwrap();
    let txn = bucket.begin().unwrap();
    txn.put(b"batch:00000000000000000001", b"committed-before-crash").unwrap();
    txn.commit().unwrap();
    std::mem::forget(db);
    std::process::exit(0);
}

#[test]
fn migrates_all_stores_and_preserves_source_bytes_and_empty_buckets() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-{}", uuid::Uuid::new_v4()));
    for name in ["metrics.mace", "telemetry/core", "telemetry/detail"] { seed(&root.join(name)); }
    let before = snapshot(&root);
    prepare(&root).unwrap();
    mark_ready(&root).unwrap();
    for (path, bytes) in before { assert_eq!(std::fs::read(root.join(path)).unwrap(), bytes); }
    for name in ["metrics.mace", "telemetry/core", "telemetry/detail"] {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join(name)).validate().unwrap()).unwrap();
        assert_eq!(db.active_buckets().len(), 3);
        let bucket = db.open_bucket("queue").unwrap();
        let view = bucket.view().unwrap();
        assert_eq!(view.get(b"counter").unwrap().slice(), b"42");
        assert_eq!(view.get(b"batch:00000000000000000003").unwrap().slice(), b"unconfirmed payload");
        assert_eq!(view.get(b"ack:00000000000000000002").unwrap().slice(), b"receipt");
        assert_eq!(view.get(b"CTOMB_hash").unwrap().slice(), b"tombstone");
        assert!(matches!(view.get(b"deleted"), Err(mace::OpCode::NotFound)));
    }
    prepare(&root).unwrap();
    // Normal runtime writes after activation must survive a restart: the
    // migration digests are not re-compared once `.ready` exists.
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        let txn = bucket.begin().unwrap(); txn.put(b"served:new", b"1").unwrap(); txn.commit().unwrap();
    }
    ensure_ready(&root).unwrap();
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"served:new").unwrap().slice(), b"1");
    }
    // A rollback to the old executable must still open the untouched source.
    let old = legacy_mace::Mace::new(legacy_options(&root.join("metrics.mace")).validate().unwrap()).unwrap();
    let bucket = old.get_bucket("metrics").unwrap();
    let txn = bucket.begin().unwrap(); txn.put(b"rollback:new", b"43").unwrap(); txn.commit().unwrap();
    drop(bucket); drop(old);
    // The next start archives the stale v2 tree and migrates the current
    // legacy data, so a retried upgrade never needs manual cleanup.
    prepare(&root).unwrap();
    let stale: Vec<_> = std::fs::read_dir(&root).unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("mace-v2.stale-"))
        .collect();
    assert_eq!(stale.len(), 1);
    {
        let archived = mace::Mace::new(mace::Options::new(root.join(&stale[0]).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = archived.open_bucket("metrics").unwrap();
        assert!(matches!(bucket.view().unwrap().get(b"rollback:new"), Err(mace::OpCode::NotFound)));
        assert_eq!(bucket.view().unwrap().get(b"served:new").unwrap().slice(), b"1");
    }
    {
        let current = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = current.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"rollback:new").unwrap().slice(), b"43");
        assert!(matches!(bucket.view().unwrap().get(b"served:new"), Err(mace::OpCode::NotFound)));
    }
    ensure_ready(&root).unwrap();
    assert!(zelvik_edge::storage_migration::ready_path(&root).exists());
    // After rollback is no longer possible, an operator may reclaim the legacy
    // stores. Their absence must not re-migrate (and empty) the active tree.
    std::fs::remove_dir_all(root.join("metrics.mace")).unwrap();
    let _ = std::fs::remove_dir_all(root.join("telemetry"));
    ensure_ready(&root).unwrap();
    {
        let current = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = current.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"rollback:new").unwrap().slice(), b"43");
    }
    assert_eq!(std::fs::read_dir(&root).unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("mace-v2.stale-"))
        .count(), 1);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn recovers_committed_legacy_wal_and_rejects_corruption_without_activation() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-wal-{}", uuid::Uuid::new_v4()));
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "legacy_wal_child"])
        .env("ZELVIK_LEGACY_MIGRATION_CHILD", root.join("telemetry/core"))
        .status().unwrap();
    assert!(status.success());
    let before = snapshot(&root);
    assert!(before.iter().any(|(path, bytes)| {
        path.file_name().and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("wal_") && !bytes.is_empty())
    }), "the fixture must leave committed records in an uncheckpointed WAL");
    prepare(&root).unwrap();
    for (path, bytes) in before { assert_eq!(std::fs::read(root.join(path)).unwrap(), bytes); }
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("telemetry/core")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("queue").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"batch:00000000000000000001").unwrap().slice(), b"committed-before-crash");
    }
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(root.join("metrics.mace/log")).unwrap();
    std::fs::write(root.join("metrics.mace/log/manifest"), b"broken manifest").unwrap();
    assert!(prepare(&root).is_err());
    assert!(!store_root(&root).exists());
    assert_eq!(std::fs::read(root.join("metrics.mace/log/manifest")).unwrap(), b"broken manifest");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn fresh_node_and_existing_v2_data_are_not_reset() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-v2-{}", uuid::Uuid::new_v4()));
    let source = root.join("metrics.mace");
    {
        let db = mace::Mace::new(mace::Options::new(&source).validate().unwrap()).unwrap();
        let bucket = db.new_bucket("metrics", Default::default()).unwrap();
        let txn = bucket.begin().unwrap(); txn.put(b"value", b"v2").unwrap(); txn.commit().unwrap();
    }
    prepare(&root).unwrap();
    prepare(&root).unwrap();
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"value").unwrap().slice(), b"v2");
    }
    std::fs::remove_dir_all(&root).unwrap();
    prepare(&root).unwrap();
    prepare(&root).unwrap();
    assert!(store_root(&root).join("migration.json").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn startup_migrates_legacy_stores_and_verifies_each_boot() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-gate-{}", uuid::Uuid::new_v4()));
    // A fresh node initializes empty v2 stores at startup.
    ensure_ready(&root).unwrap();
    assert!(zelvik_edge::storage_migration::ready_path(&root).exists());
    ensure_ready(&root).unwrap();
    // The first start of a new binary cannot run a candidate preflight under
    // the old supervisor, so startup itself migrates legacy data.
    seed(&root.join("metrics.mace"));
    std::fs::remove_dir_all(store_root(&root)).unwrap();
    ensure_ready(&root).unwrap();
    assert!(zelvik_edge::storage_migration::ready_path(&root).exists());
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"counter").unwrap().slice(), b"42");
    }
    // A readiness marker without its receipt is corruption, not a pass.
    std::fs::remove_file(store_root(&root).join("migration.json")).unwrap();
    let error = ensure_ready(&root).unwrap_err().to_string();
    assert!(error.contains("without a migration receipt"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn unmarked_activation_is_verified_against_the_receipt() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-unmarked-{}", uuid::Uuid::new_v4()));
    seed(&root.join("metrics.mace"));
    prepare(&root).unwrap();
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        let txn = bucket.begin().unwrap(); txn.put(b"tampered", b"1").unwrap(); txn.commit().unwrap();
    }
    let error = prepare(&root).unwrap_err().to_string();
    assert!(error.contains("verification failed"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_legacy_removal_is_refused_instead_of_emptying_stores() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-partial-{}", uuid::Uuid::new_v4()));
    seed(&root.join("metrics.mace"));
    seed(&root.join("telemetry/core"));
    prepare(&root).unwrap();
    mark_ready(&root).unwrap();
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        let txn = bucket.begin().unwrap(); txn.put(b"live", b"1").unwrap(); txn.commit().unwrap();
    }
    std::fs::remove_dir_all(root.join("telemetry")).unwrap();
    let error = ensure_ready(&root).unwrap_err().to_string();
    assert!(error.contains("partially removed"), "{error}");
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"live").unwrap().slice(), b"1");
    }
    assert_eq!(stale_count(&root), 0);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_rebuild_keeps_the_active_tree() {
    let root = std::env::temp_dir().join(format!("zelvik-migration-rebuild-{}", uuid::Uuid::new_v4()));
    for name in ["metrics.mace", "telemetry/core", "telemetry/detail"] { seed(&root.join(name)); }
    prepare(&root).unwrap();
    mark_ready(&root).unwrap();
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        let txn = bucket.begin().unwrap(); txn.put(b"live", b"1").unwrap(); txn.commit().unwrap();
    }
    // A rollback writes the legacy store; corrupt it so the rebuild fails.
    let old = legacy_mace::Mace::new(legacy_options(&root.join("metrics.mace")).validate().unwrap()).unwrap();
    let bucket = old.get_bucket("metrics").unwrap();
    let txn = bucket.begin().unwrap(); txn.put(b"rollback:new", b"43").unwrap(); txn.commit().unwrap();
    drop(bucket); drop(old);
    std::fs::write(root.join("metrics.mace/log/manifest"), b"broken manifest").unwrap();
    assert!(prepare(&root).is_err());
    // The previous tree was never archived or replaced, so it stays usable.
    {
        let db = mace::Mace::new(mace::Options::new(store_root(&root).join("metrics.mace")).validate().unwrap()).unwrap();
        let bucket = db.open_bucket("metrics").unwrap();
        assert_eq!(bucket.view().unwrap().get(b"live").unwrap().slice(), b"1");
    }
    assert_eq!(stale_count(&root), 0);
    std::fs::remove_dir_all(root).unwrap();
}

fn stale_count(root: &Path) -> usize {
    std::fs::read_dir(root).unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with("mace-v2.stale-"))
        .count()
}

fn snapshot(root: &Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(root: &Path, path: &Path, result: &mut Vec<(std::path::PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { visit(root, &path, result); }
            else { result.push((path.strip_prefix(root).unwrap().to_owned(), std::fs::read(&path).unwrap())); }
        }
    }
    let mut result = Vec::new(); visit(root, root, &mut result); result
}
