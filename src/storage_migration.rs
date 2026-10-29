//! Offline, copy-on-write migration of the node's durable stores.
//!
//! `run_node` calls [`ensure_ready`] after acquiring the node PID lock and
//! before opening any store; the installer and `zelvik-edge storage-migrate`
//! run [`prepare`] while the service is stopped. Old directories remain
//! untouched so the previous executable can still start after rollback.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const STORES: [&str; 3] = ["metrics.mace", "telemetry/core", "telemetry/detail"];
const ROOT: &str = "mace-v2";
const READY: &str = "migration.ready";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SourceSnapshot {
    name: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct MigrationReceipt {
    sources: Vec<SourceSnapshot>,
    stores: Vec<(String, Vec<BucketDigest>)>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct BucketDigest {
    name: String,
    records: u64,
    sha256: String,
}

pub fn store_root(data: &Path) -> PathBuf {
    data.join(ROOT)
}

pub fn ready_path(data: &Path) -> PathBuf {
    store_root(data).join(READY)
}

pub fn mark_ready(data: &Path) -> anyhow::Result<()> {
    let path = ready_path(data);
    let temp = path.with_extension(format!("ready.{}", std::process::id()));
    File::create(&temp)?.sync_all()?;
    fs::rename(&temp, &path)?;
    sync_directory(&store_root(data))
}

/// Prepare the durable stores before this executable opens any of them.
///
/// A completed migration is verified against its receipt on every start. When
/// legacy stores exist without a completed migration, the copy-on-write
/// migration runs here: an in-place upgrade is driven by the old executable's
/// supervisor, which cannot run a candidate preflight, so the first start of
/// this binary is the only point after the old process stopped. The legacy
/// stores stay untouched, so a rollback to the previous executable still works.
pub fn ensure_ready(data: &Path) -> anyhow::Result<()> {
    if ready_path(data).exists() {
        anyhow::ensure!(store_root(data).join("migration.json").exists(),
            "Mace migration readiness marker exists without a migration receipt; run `zelvik-edge storage-migrate` while the service is stopped");
    }
    prepare(data)?;
    mark_ready(data)
}

fn source_snapshots(data: &Path) -> anyhow::Result<Vec<SourceSnapshot>> {
    STORES.iter().map(|name| {
        let root = data.join(name);
        Ok(SourceSnapshot {
            name: name.to_string(),
            sha256: tree_digest(&root)?,
            bytes: store_bytes(&root)?,
        })
    }).collect()
}

fn store_bytes(root: &Path) -> anyhow::Result<u64> {
    files_in(root)?.into_iter().try_fold(0u64, |sum, relative| {
        Ok::<_, anyhow::Error>(sum.saturating_add(fs::metadata(root.join(relative))?.len()))
    })
}

fn files_in(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    fn visit(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            anyhow::ensure!(!kind.is_symlink(), "database migration refuses symlink {}", entry.path().display());
            if kind.is_dir() {
                visit(root, &entry.path(), files)?;
            } else {
                anyhow::ensure!(kind.is_file(), "unsupported database file {}", entry.path().display());
                files.push(entry.path().strip_prefix(root)?.to_owned());
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    if root.exists() { visit(root, root, &mut files)?; }
    files.sort();
    Ok(files)
}

fn tree_digest(root: &Path) -> anyhow::Result<String> {
    let mut digest = Sha256::new();
    digest.update([u8::from(root.exists())]);
    let mut buffer = [0u8; 64 * 1024];
    for relative in files_in(root)? {
        let name = relative.as_os_str().as_encoded_bytes();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name);
        let mut file = File::open(root.join(&relative))?;
        digest.update(file.metadata()?.len().to_be_bytes());
        loop {
            let len = file.read(&mut buffer)?;
            if len == 0 { break; }
            digest.update(&buffer[..len]);
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn copy_store(source: &Path, destination: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(destination)?;
    for relative in files_in(source)? {
        let target = destination.join(&relative);
        fs::create_dir_all(target.parent().context("database file has no parent")?)?;
        fs::copy(source.join(relative), &target)?;
        File::open(target)?.sync_all()?;
    }
    sync_directory(destination)
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn sync_tree_directories(root: &Path) -> anyhow::Result<()> {
    let mut dirs = vec![root.to_owned()];
    for relative in files_in(root)? {
        let mut current = root.join(relative);
        current.pop();
        while current != root {
            dirs.push(current.clone());
            current.pop();
        }
    }
    dirs.sort();
    dirs.dedup();
    for dir in dirs.into_iter().rev() {
        sync_directory(&dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> anyhow::Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs initializes the caller-provided structure on success.
    let result = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    let bytes = u128::from(stat.f_bavail).saturating_mul(u128::from(stat.f_frsize));
    Ok(u64::try_from(bytes).unwrap_or(u64::MAX))
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> anyhow::Result<u64> {
    anyhow::bail!("offline Mace migration requires a filesystem free-space query on this platform")
}

fn record_digest(digest: &mut Sha256, key: &[u8], value: &[u8]) {
    digest.update((key.len() as u64).to_be_bytes());
    digest.update(key);
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn legacy_options(path: &Path) -> legacy_mace::Options {
    let mut options = legacy_mace::Options::new(path);
    options.concurrent_write = legacy_mace::Options::CONCURRENT_WRITE;
    options.sync_on_write = true;
    options.lru_capacity = 8 << 20;
    options.stat_mask_cache_count = 128;
    options.data_handle_cache_capacity = 16;
    options.blob_handle_cache_capacity = 8;
    options.wal_buffer_size = 4 << 20;
    options.wal_file_size = 4 << 20;
    options.gc_timeout = 3_600_000;
    options.gc_eager = false;
    options.data_garbage_ratio = 100;
    options.blob_garbage_ratio = 100;
    options.checkpoint_nudge_ms = 0;
    // Match the deployed 4.7 runtime: an incomplete WAL tail (crash or power
    // loss before that record became durable) is truncated on our private
    // copy. The legacy store itself is never opened for writing.
    options.truncate_corrupted_wal = true;
    options.tmp_store = false;
    options
}

fn destination_options(path: &Path, sync_on_write: bool) -> mace::Options {
    let mut options = mace::Options::new(path);
    options.concurrent_write = mace::Options::CONCURRENT_WRITE;
    options.sync_on_write = sync_on_write;
    options.lru_capacity = 8 << 20;
    options.stat_mask_cache_count = 128;
    options.data_handle_cache_capacity = 16;
    options.blob_handle_cache_capacity = 8;
    options.wal_buffer_size = 4 << 20;
    options.wal_file_size = 4 << 20;
    options
}


fn bucket_options() -> mace::BucketOptions {
    mace::BucketOptions {
        cache_capacity: 8 << 20,
        pool_capacity: 8 << 20,
        checkpoint_size: 2 << 20,
        ..Default::default()
    }
}

fn verify_store(path: &Path) -> anyhow::Result<Vec<BucketDigest>> {
    let db = mace::Mace::new(destination_options(path, true).validate()?)?;
    let mut names = db.active_buckets();
    names.sort();
    let mut digests = Vec::new();
    for name in names {
        let bucket = db.open_bucket(&name)?;
        let view = bucket.view()?;
        let mut digest = Sha256::new();
        let mut records = 0;
        let mut iter = view.range::<&[u8], _>(..);
        while let Some(item) = iter.try_next()? {
            record_digest(&mut digest, item.key(), item.val());
            records += 1;
        }
        digests.push(BucketDigest { name, records, sha256: hex::encode(digest.finalize()) });
    }
    Ok(digests)
}

fn migrate_store(source_copy: &Path, target: &Path) -> anyhow::Result<Vec<BucketDigest>> {
    if !source_copy.join("log/manifest").exists() {
        anyhow::ensure!(files_in(source_copy)?.is_empty(),
            "database contains files but no manifest: {}", source_copy.display());
        fs::create_dir_all(target)?;
        return Ok(Vec::new());
    }
    let report = mace::check_database(&destination_options(source_copy, true));
    if !report.diagnostics.iter().any(|d| d.category == mace::CheckCategory::UnsupportedMetadataFormat) {
        anyhow::ensure!(report.is_ok(), "Mace v2 validation failed: {report:?}");
        copy_store(source_copy, target)?;
        return verify_store(target);
    }
    // This is the exact reader used by 4.7, opened only on our private copy.
    // Its recovery replays committed WAL before iteration. Deleted keys stay
    // absent; application tombstones, receipts and producer state are values.
    let old = legacy_mace::Mace::new(legacy_options(source_copy).validate()?)?;
    let new = mace::Mace::new(destination_options(target, true).validate()?)?;
    let mut names = old.active_buckets();
    names.sort();
    let mut expected = Vec::new();
    for name in names {
        let source = old.get_bucket(&name)?;
        let destination = new.new_bucket(&name, bucket_options())?;
        let view = source.view()?;
        let mut digest = Sha256::new();
        let mut records = 0;
        let mut bytes = 0;
        let mut txn = destination.begin()?;
        for item in view.range::<&[u8], _>(..) {
            let (key, value) = (item.key(), item.val());
            txn.put(key, value)?;
            record_digest(&mut digest, key, value);
            records += 1;
            bytes += key.len() + value.len();
            if bytes >= 1 << 20 {
                txn.commit()?;
                txn = destination.begin()?;
                bytes = 0;
            }
        }
        txn.commit()?;
        expected.push(BucketDigest { name, records, sha256: hex::encode(digest.finalize()) });
    }
    new.sync()?;
    drop(new);
    drop(old);
    let actual = verify_store(target)?;
    anyhow::ensure!(actual == expected, "migrated database content verification failed");
    let report = mace::check_database(&destination_options(target, true));
    anyhow::ensure!(report.is_ok(), "migrated database integrity check failed: {report:?}");
    Ok(actual)
}

/// Activate all stores together, never an empty replacement for an unreadable
/// database. A failed attempt leaves the previous tree (if any) untouched and
/// one unpublished staging tree behind.
pub fn prepare(data: &Path) -> anyhow::Result<()> {
    let destination = store_root(data);
    let sources = source_snapshots(data)?;
    let mut replace = None;
    if destination.exists() {
        let receipt: MigrationReceipt = serde_json::from_slice(
            &fs::read(destination.join("migration.json"))
                .context("Mace migration receipt missing; preserve both database generations")?,
        )?;
        if receipt.sources != sources {
            if sources.iter().all(|source| source.bytes == 0) {
                // The operator reclaimed the legacy stores after activation.
                // Nothing could have written to them since, so the activated
                // tree stays authoritative. A failed earlier attempt may have
                // left one stage behind; it is no longer useful.
                remove_stale_stages(data)?;
                return Ok(());
            }
            // Refuse when a store that held data at migration is gone now:
            // rebuilding it would create an empty replacement and the active
            // tree is the only copy of that data.
            let removed = receipt.sources.iter().any(|was| {
                was.bytes > 0
                    && sources.iter()
                        .find(|now| now.name == was.name)
                        .is_none_or(|now| now.bytes == 0)
            });
            anyhow::ensure!(!removed,
                "legacy Mace stores were partially removed after migration; restore them or remove metrics.mace and telemetry/ together to keep the active data; refusing to rebuild deleted stores as empty"
            );
            // A rollback wrote the legacy stores again. Build and verify the
            // replacement first; the current tree is archived only after the
            // new one is ready, so a failed rebuild leaves the healthy tree in
            // place (removing the legacy stores then keeps it usable).
            replace = Some(destination.clone());
        } else {
            // The digests describe the store contents at migration time. They
            // are only a live invariant before the first activation; afterwards
            // the service legitimately writes new records, so a boot must not
            // compare them again.
            if !ready_path(data).exists() {
                for (name, expected) in &receipt.stores {
                    let actual = verify_store(&destination.join(name))?;
                    anyhow::ensure!(&actual == expected,
                        "activated Mace v2 store verification failed for {name}; preserve both generations");
                }
            }
            return Ok(());
        }
    }
    fs::create_dir_all(data)?;
    // A failed attempt leaves its staged copies behind. A retry (systemd
    // restarts on failure) replaces them; keeping every attempt would add one
    // full copy of the legacy stores per retry until the disk fills. The
    // untouched legacy stores remain the recovery source.
    remove_stale_stages(data)?;
    let source_bytes = sources.iter().fold(0u64, |sum, source| sum.saturating_add(source.bytes));
    let required = source_bytes.saturating_mul(2).saturating_add(256 << 20);
    anyhow::ensure!(available_bytes(data)? >= required,
        "insufficient free space for lossless Mace migration: need at least {required} bytes in addition to the legacy stores");
    let stage = data.join(format!(".mace-migration-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage)?;
    // No source database is ever opened for writing here.
    let mut stores = Vec::new();
    for name in STORES {
        let source = data.join(name);
        let copied = stage.join("source").join(name);
        copy_store(&source, &copied)?;
        let digests = migrate_store(&copied, &stage.join(name))
            .with_context(|| format!("offline migration failed for {name}; source preserved at {}", source.display()))?;
        stores.push((name.to_string(), digests));
    }
    anyhow::ensure!(source_snapshots(data)? == sources,
        "source databases changed while migrating; refusing to activate inconsistent snapshot");
    // Every migrated store is already verified against its source digest. The
    // private copy is no longer needed and would otherwise double the retained
    // footprint of the activated tree; the original legacy stores stay intact.
    fs::remove_dir_all(stage.join("source"))?;
    let mut receipt = File::create(stage.join("migration.json"))?;
    receipt.write_all(&serde_json::to_vec_pretty(&MigrationReceipt { sources, stores })?)?;
    receipt.sync_all()?;
    // Persist every nested database directory before activation. The receipt is
    // not trusted on a later boot unless each activated store is reopened and
    // its bucket digests are checked again.
    sync_tree_directories(&stage)?;
    if let Some(previous) = replace {
        let stale = data.join(format!("{ROOT}.stale-{}", uuid::Uuid::new_v4()));
        fs::rename(&previous, &stale)?;
        fs::rename(&stage, &destination)?;
        sync_directory(data)?;
        tracing::warn!(path = %stale.display(),
            "legacy Mace stores changed after migration; archived the previous Mace v2 tree and activated a rebuilt one");
    } else {
        fs::rename(&stage, &destination)?;
        sync_directory(data)?;
    }
    tracing::info!(path = %destination.display(), "Mace migration verified and activated; original stores preserved");
    Ok(())
}

fn remove_stale_stages(data: &Path) -> anyhow::Result<()> {
    for entry in fs::read_dir(data)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry.file_name().to_string_lossy().starts_with(".mace-migration-")
        {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
