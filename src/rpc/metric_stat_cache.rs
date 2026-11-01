//! Durable cumulative snapshot store for the `metric_stat` reporter.
//!
//! The reporter no longer sends incremental values scoped to the process's
//! dedup cache. Each snapshot row is the full cumulative value of one metric
//! identity — `(item, version, period key, period start, server, resolved
//! keys)` — and carries a strictly increasing revision. The control plane's
//! `ReplacingMergeTree(revision)` keeps the highest revision, so replaying a
//! row after a crash or a duplicate spool append is idempotent. There is no
//! receiver-side `keepKeys` state and no assumption that the receiver still
//! holds a previous batch.
//!
//! Crash safety:
//! - New projections are merged into the snapshot store and the checkpoint is
//!   fsynced before any upload is attempted.
//! - Durable handoff advances `uploaded_revision` and releases those rows. The
//!   next checkpoint records their removal and the revision high-water mark;
//!   if that update is lost, replay uses the same hash and revision.
//! - Each row instance has an immutable incarnation (its creation revision)
//!   that is part of the upload hash. A row recreated by a late event after
//!   its clean instance was pruned therefore uploads under a new hash, so the
//!   receiver sums the old total and the new deltas instead of replacing the
//!   old cumulative value.
//! - A missing checkpoint in a new directory is the explicit first cutover and
//!   creates a fresh random epoch. Once a checkpoint exists, an unreadable,
//!   truncated, oversized or inconsistent checkpoint is an error: the file is
//!   left in place and the reporter refuses to invent a new epoch/revision 0,
//!   which would double-count on the receiver.
//!
//! All state is bounded. Rows live in one byte/entry-bounded map, recovery
//! reads at most one checkpoint with a hard size cap, and oversized records
//! that cannot fit the wire are written to a durable local quarantine rather
//! than silently dropped.

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::warn;

use crate::pb;

/// Upper bound on retained dedup entries for a typical node. A node with more
/// distinct snapshot identities than this keeps reporting correctly; new keys
/// beyond the cap are refused whole and counted as cardinality drops.
pub(crate) const DEFAULT_MAX_ENTRIES: usize = 100_000;
/// Upper bound on estimated live snapshot bytes.
pub(crate) const DEFAULT_MAX_ESTIMATED_BYTES: u64 = 64 * 1024 * 1024;
/// One checkpoint must fit this. A larger checkpoint is an explicit reporter
/// error, never a partial load and never a reset.
pub(crate) const DEFAULT_MAX_CHECKPOINT_BYTES: u64 = 96 * 1024 * 1024;
/// On-disk ceiling for records that cannot fit any wire chunk even alone. Files
/// are retained for operator replay; a full directory blocks only the oversized
/// record path.
pub(crate) const DEFAULT_MAX_QUARANTINE_BYTES: u64 = 256 * 1024 * 1024;
/// One upload page is bounded by both row count and cloned-row bytes. A single
/// pathological key still travels alone, but 1024 large keys cannot be cloned
/// into one page.
pub(crate) const DEFAULT_DIRTY_PAGE_BYTES: u64 = 2 * 1024 * 1024;

const CHECKPOINT_VERSION: u32 = 2;
const CHECKPOINT_NAME: &str = "metric-snapshot.checkpoint.json";

/// Observable size of the metric snapshot state, also consumed by memory
/// governance and node status. The field names are kept stable for existing
/// status consumers; their meaning now follows the cumulative snapshot model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricValuesCacheStats {
    /// Snapshot rows currently retained.
    pub entries: usize,
    /// Estimated live snapshot bytes plus any retained in-memory payloads.
    pub estimated_bytes: u64,
    /// Configured row ceiling.
    pub max_entries: usize,
    /// Combined snapshot + recovery byte ceiling.
    pub max_estimated_bytes: u64,
    /// Snapshot-only estimate ceiling.
    pub dedup_max_estimated_bytes: u64,
    /// Checkpoint-only ceiling (dirty rows are part of the checkpoint).
    pub max_pending_bytes: u64,
    /// Rows released after durable handoff to the spool or quarantine.
    pub evicted_entries: u64,
    /// Records that cannot fit the wire even alone and were written to durable
    /// local quarantine. Deliberately not deleted.
    pub terminal_oversize_records: u64,
    /// Non-capacity spool refusals observed; the dirty snapshot row is retained
    /// and retried.
    pub terminal_failed_uploads: u64,
    /// Snapshot rows with `uploaded_revision < revision`.
    pub pending_payloads: usize,
    /// Estimated bytes of those dirty rows.
    pub pending_bytes: u64,
    /// True while the checkpoint could not be written or the recovery ceiling
    /// was reached; no new projections are accepted until it clears.
    pub backpressured: bool,
    /// Bytes written to the durable quarantine directory.
    pub quarantined_bytes: u64,
}

static PUBLISHED_STATS: LazyLock<RwLock<MetricValuesCacheStats>> =
    LazyLock::new(|| RwLock::new(MetricValuesCacheStats::default()));

pub fn metric_values_cache_stats() -> MetricValuesCacheStats {
    *PUBLISHED_STATS.read()
}

fn publish(stats: MetricValuesCacheStats) {
    *PUBLISHED_STATS.write() = stats;
}

/// True while the reporter cannot open its checkpoint (for example a
/// permission error that an operator has not fixed yet). The projection
/// aggregator reads this before admitting a new key so the shared pool does not
/// keep filling while nothing can drain it. Existing keys still accumulate;
/// only new identities are rejected, and every rejection is counted.
static STORE_UNAVAILABLE: AtomicBool = AtomicBool::new(false);

pub(crate) fn metric_snapshot_store_unavailable() -> bool {
    STORE_UNAVAILABLE.load(Ordering::Acquire)
}

pub(crate) fn set_metric_snapshot_store_unavailable(unavailable: bool) {
    STORE_UNAVAILABLE.store(unavailable, Ordering::Release);
    if unavailable {
        PUBLISHED_STATS.write().backpressured = true;
    }
}

/// Identity of one cumulative metric row. `time_key` and `window_start` are
/// the period identity; `window_end` is deliberately not part of the identity
/// because it advances to the last real event in the period while the row stays
/// the same row.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub(crate) struct SnapshotKey {
    pub item_id: i64,
    pub version: i32,
    pub time_key: String,
    pub window_start: i64,
    /// Full period boundary, independent of the wire `window_end`. Pruning uses
    /// this; the wire window may end just after the last real event.
    pub period_end: i64,
    pub server_id: i64,
    pub keys: Vec<String>,
}

impl SnapshotKey {
    /// Wire identity of one accumulated row *instance*.
    ///
    /// `incarnation` is the global revision assigned when the row was created
    /// and never changes while that row exists. A row released after durable
    /// handoff and later recreated by another event gets a new incarnation, so it
    /// uploads under a different hash. The control plane therefore keeps the
    /// old instance's total and adds the new instance's deltas instead of
    /// replacing the old cumulative value with a smaller one.
    pub(crate) fn hash(&self, epoch: &str, incarnation: u64) -> String {
        let keys_data = self.keys.join("$EDGE$");
        // The opaque version/epoch suffix keeps cumulative snapshots from
        // colliding with revision-0 rows migrated from the legacy incremental
        // table, and the incarnation makes each row instance distinct.
        let hash_raw = format!(
            "{}@{}@{}@{}@{}@snapshot-v3-{}@{}",
            self.server_id,
            keys_data,
            self.time_key,
            self.version,
            self.item_id,
            epoch,
            incarnation
        );
        crate::utils::fnv_hash64(&hash_raw).to_string()
    }
}

/// One drained projection converted to a cumulative delta.
#[derive(Debug)]
pub(crate) struct SnapshotDelta {
    pub key: SnapshotKey,
    pub window_end: i64,
    pub value: f32,
    /// Charge already held by the projection pool for this row. It transfers
    /// with the row; it is never charged twice. A zero value means the caller
    /// already accounted for it (tests).
    pub charge: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotRow {
    key: SnapshotKey,
    window_end: i64,
    value: f32,
    revision: u64,
    /// Immutable identity of this row instance, fixed at creation. It is part
    /// of the upload hash and must survive checkpoint restart unchanged.
    incarnation: u64,
    uploaded_revision: u64,
    #[serde(skip)]
    charge: u64,
    #[serde(skip)]
    estimated_bytes: u64,
}

/// A dirty row ready to be turned into one wire record.
#[derive(Clone, Debug)]
pub(crate) struct DirtySnapshotRow {
    pub key: SnapshotKey,
    pub window_end: i64,
    pub value: f32,
    pub revision: u64,
    /// Row-instance identity included in the upload hash.
    pub incarnation: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MergeOutcome {
    pub updated: usize,
    pub created: usize,
    pub dropped: usize,
    /// Pool bytes released because the delta merged into an existing row or
    /// was refused.
    pub released_bytes: u64,
}

/// Checkpoint metadata is the first JSON line; every following line is one
/// `SnapshotRow`. The line format lets recovery decode one bounded row at a
/// time instead of materializing the whole file as a JSON document in addition
/// to the live row map.
#[derive(Serialize, Deserialize)]
struct CheckpointMeta {
    checkpoint_version: u32,
    epoch: String,
    next_revision: u64,
}

/// Reporter state. Only the reporter owns it; the projection aggregators are
/// drained into it on the reporter task.
pub(crate) struct MetricStatReporterState {
    rows: BTreeMap<SnapshotKey, SnapshotRow>,
    memory_bytes: u64,
    epoch: String,
    next_revision: u64,
    max_entries: usize,
    max_estimated_bytes: u64,
    max_checkpoint_bytes: u64,
    checkpoint_path: PathBuf,
    quarantine_dir: PathBuf,
    quarantine_max_bytes: u64,
    quarantined_records: u64,
    quarantined_bytes: u64,
    terminal_failed_uploads: u64,
    evicted_entries: u64,
    backpressured: bool,
    checkpoint_error: Option<String>,
}

impl MetricStatReporterState {
    /// Open (or explicitly create) the checkpoint under `base_dir`.
    ///
    /// A missing checkpoint is the only path that creates a new epoch. Any
    /// other load failure is returned to the caller and the file is untouched.
    pub(crate) fn open(base_dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let base_dir = base_dir.into();
        std::fs::create_dir_all(&base_dir).map_err(|e| {
            anyhow::anyhow!(
                "create metric snapshot directory {}: {e}",
                base_dir.display()
            )
        })?;
        let checkpoint_path = base_dir.join(CHECKPOINT_NAME);
        let quarantine_dir = base_dir.join("quarantine");
        let mut state = Self {
            rows: BTreeMap::new(),
            memory_bytes: 0,
            epoch: String::new(),
            next_revision: 0,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_estimated_bytes: DEFAULT_MAX_ESTIMATED_BYTES,
            max_checkpoint_bytes: DEFAULT_MAX_CHECKPOINT_BYTES,
            checkpoint_path,
            quarantine_dir,
            quarantine_max_bytes: DEFAULT_MAX_QUARANTINE_BYTES,
            quarantined_records: 0,
            quarantined_bytes: 0,
            terminal_failed_uploads: 0,
            evicted_entries: 0,
            backpressured: false,
            checkpoint_error: None,
        };
        state.quarantined_bytes = dir_usage(&state.quarantine_dir).map_err(|e| {
            anyhow::anyhow!(
                "read metric stat quarantine usage {}: {e}",
                state.quarantine_dir.display()
            )
        })?;
        warn_legacy_quarantine(&base_dir);

        if state.checkpoint_path.exists() {
            state.load_checkpoint()?;
        } else {
            // Explicit first cutover: empty state, fresh epoch, revision starts
            // at 1. Once this file exists it is authoritative forever.
            state.epoch = uuid::Uuid::new_v4().to_string();
            state.next_revision = 0;
            state.write_checkpoint_sync()?;
        }
        set_metric_snapshot_store_unavailable(false);
        state.publish_stats();
        Ok(state)
    }

    fn load_checkpoint(&mut self) -> anyhow::Result<()> {
        let mut charged = 0u64;
        match self.load_checkpoint_rows(&mut charged) {
            Ok((rows, memory_bytes, next_revision, epoch)) => {
                self.rows = rows;
                self.memory_bytes = memory_bytes;
                self.next_revision = next_revision;
                self.epoch = epoch;
                Ok(())
            }
            Err(e) => {
                // A failed recovery keeps the checkpoint and returns the pool to
                // its pre-load state; the reporter retries the open.
                release_charge(charged);
                Err(e)
            }
        }
    }

    fn load_checkpoint_rows(
        &self,
        charged: &mut u64,
    ) -> anyhow::Result<(BTreeMap<SnapshotKey, SnapshotRow>, u64, u64, String)> {
        let meta = std::fs::metadata(&self.checkpoint_path).map_err(|e| {
            anyhow::anyhow!(
                "stat metric snapshot checkpoint {}: {e}",
                self.checkpoint_path.display()
            )
        })?;
        if meta.len() > self.max_checkpoint_bytes {
            return Err(anyhow::anyhow!(
                "metric snapshot checkpoint {} is {} bytes, above the {} byte recovery cap; file retained",
                self.checkpoint_path.display(),
                meta.len(),
                self.max_checkpoint_bytes
            ));
        }
        let file = std::fs::File::open(&self.checkpoint_path).map_err(|e| {
            anyhow::anyhow!(
                "read metric snapshot checkpoint {}: {e}",
                self.checkpoint_path.display()
            )
        })?;
        let mut reader = std::io::BufReader::new(file);
        let header = read_checkpoint_line(&mut reader, self.max_checkpoint_bytes)?;
        let header: CheckpointMeta = serde_json::from_slice(&header).map_err(|e| {
            anyhow::anyhow!(
                "decode metric snapshot checkpoint header {}: {e}; file retained, refusing to reset the epoch",
                self.checkpoint_path.display()
            )
        })?;
        if header.checkpoint_version != CHECKPOINT_VERSION {
            return Err(anyhow::anyhow!(
                "metric snapshot checkpoint {} has unsupported version {} (expected {}); file retained",
                self.checkpoint_path.display(),
                header.checkpoint_version,
                CHECKPOINT_VERSION
            ));
        }
        if header.epoch.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "metric snapshot checkpoint {} has an empty epoch; file retained",
                self.checkpoint_path.display()
            ));
        }
        let mut rows = BTreeMap::new();
        let mut memory_bytes = 0u64;
        let mut max_revision = 0u64;
        loop {
            let line = read_checkpoint_line(&mut reader, self.max_checkpoint_bytes)?;
            if line.is_empty() {
                break;
            }
            let mut row: SnapshotRow = serde_json::from_slice(&line).map_err(|e| {
                anyhow::anyhow!(
                    "decode a metric snapshot row in {}: {e}; file retained",
                    self.checkpoint_path.display()
                )
            })?;
            if row.revision == 0 {
                return Err(anyhow::anyhow!(
                    "metric snapshot checkpoint {} contains revision 0; file retained",
                    self.checkpoint_path.display()
                ));
            }
            if row.incarnation == 0 {
                return Err(anyhow::anyhow!(
                    "metric snapshot checkpoint {} contains incarnation 0; file retained",
                    self.checkpoint_path.display()
                ));
            }
            if row.uploaded_revision > row.revision {
                return Err(anyhow::anyhow!(
                    "metric snapshot checkpoint {} has uploaded revision {} above row revision {}",
                    self.checkpoint_path.display(),
                    row.uploaded_revision,
                    row.revision
                ));
            }
            let estimated = snapshot_row_cost(&row.key);
            // Recovered rows consume the same shared pool as live projections.
            // Charge before inserting; a refused charge keeps the checkpoint and
            // turns into explicit reporter backpressure instead of an
            // unaccounted map.
            if !crate::metrics::projection::METRIC_PROJECTION_POOL.try_charge(estimated) {
                return Err(anyhow::anyhow!(
                    "metric snapshot checkpoint {} needs {} bytes but the shared metric pool refused the recovery charge; file retained",
                    self.checkpoint_path.display(),
                    estimated
                ));
            }
            *charged = (*charged).saturating_add(estimated);
            row.charge = estimated;
            row.estimated_bytes = estimated;
            max_revision = max_revision.max(row.revision).max(row.incarnation);
            memory_bytes = memory_bytes.saturating_add(estimated);
            if rows.insert(row.key.clone(), row).is_some() {
                return Err(anyhow::anyhow!(
                    "metric snapshot checkpoint {} contains a duplicate snapshot key",
                    self.checkpoint_path.display()
                ));
            }
            // The checkpoint file itself is the hard recovery bound. Accepted
            // projections are bounded by the projection charge pool before they
            // reach this store; refusing them again on reload would strand
            // already-accepted rows instead of preserving them.
        }
        Ok((
            rows,
            memory_bytes,
            header.next_revision.max(max_revision),
            header.epoch,
        ))
    }

    fn checkpoint_meta(&self) -> CheckpointMeta {
        CheckpointMeta {
            checkpoint_version: CHECKPOINT_VERSION,
            epoch: self.epoch.clone(),
            next_revision: self.next_revision,
        }
    }

    /// Test/first-cutover helper. Production persistence streams rows instead
    /// of building this full byte buffer.
    fn encode_checkpoint(&self) -> anyhow::Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(&self.checkpoint_meta())
            .map_err(|e| anyhow::anyhow!("encode metric snapshot checkpoint: {e}"))?;
        bytes.push(b'\n');
        for row in self.rows.values() {
            serde_json::to_writer(&mut bytes, row)
                .map_err(|e| anyhow::anyhow!("encode metric snapshot row: {e}"))?;
            bytes.push(b'\n');
        }
        if bytes.len() as u64 > self.max_checkpoint_bytes {
            anyhow::bail!(
                "metric snapshot checkpoint is {} bytes, above the {} byte cap; retained in memory",
                bytes.len(),
                self.max_checkpoint_bytes
            );
        }
        Ok(bytes)
    }

    fn write_checkpoint_sync(&self) -> anyhow::Result<()> {
        let bytes = self.encode_checkpoint()?;
        write_atomic(&self.checkpoint_path, &bytes)
    }

    /// Stream the cumulative state, one row at a time, before any upload.
    /// Failure is backpressure: dirty rows stay in memory and are retried,
    /// never dropped. The temporary file is removed on failure so a partial
    /// checkpoint can never replace the last complete one.
    pub(crate) async fn persist_checkpoint(&mut self) -> anyhow::Result<()> {
        let result = self.write_checkpoint_stream().await;
        match result {
            Ok(()) => {
                self.checkpoint_error = None;
                Ok(())
            }
            Err(e) => {
                self.checkpoint_error = Some(e.to_string());
                Err(e)
            }
        }
    }

    async fn write_checkpoint_stream(&self) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let tmp = checkpoint_temp_path(&self.checkpoint_path);
        let write = async {
            let mut file = tokio::fs::File::create(&tmp).await?;
            let mut used = 0u64;
            let header = serde_json::to_vec(&self.checkpoint_meta())
                .map_err(|e| anyhow::anyhow!("encode metric snapshot checkpoint header: {e}"))?;
            used = used.saturating_add(header.len() as u64 + 1);
            if used > self.max_checkpoint_bytes {
                anyhow::bail!(
                    "metric snapshot checkpoint header exceeds the {} byte cap",
                    self.max_checkpoint_bytes
                );
            }
            file.write_all(&header).await?;
            file.write_all(b"\n").await?;
            for row in self.rows.values() {
                let line = serde_json::to_vec(row)
                    .map_err(|e| anyhow::anyhow!("encode metric snapshot row: {e}"))?;
                used = used.saturating_add(line.len() as u64 + 1);
                if used > self.max_checkpoint_bytes {
                    anyhow::bail!(
                        "metric snapshot checkpoint is above the {} byte cap; retained in memory",
                        self.max_checkpoint_bytes
                    );
                }
                file.write_all(&line).await?;
                file.write_all(b"\n").await?;
            }
            file.sync_all().await?;
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(e) = write {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        if let Err(e) = tokio::fs::rename(&tmp, &self.checkpoint_path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(anyhow::anyhow!(
                "rename metric snapshot checkpoint {}: {e}",
                self.checkpoint_path.display()
            ));
        }
        if let Some(parent) = self.checkpoint_path.parent() {
            let parent = parent.to_path_buf();
            crate::worker_reclaim::spawn_storage_work(move || {
                let dir = std::fs::File::open(&parent).map_err(|e| {
                    anyhow::anyhow!(
                        "open metric snapshot checkpoint directory {}: {e}",
                        parent.display()
                    )
                })?;
                dir.sync_all().map_err(|e| {
                    anyhow::anyhow!(
                        "sync metric snapshot checkpoint directory {}: {e}",
                        parent.display()
                    )
                })?;
                Ok::<(), anyhow::Error>(())
            })
            .await
            .map_err(|e| {
                anyhow::anyhow!("metric snapshot checkpoint directory sync task failed: {e}")
            })??;
        }
        Ok(())
    }

    pub(crate) fn epoch(&self) -> &str {
        &self.epoch
    }

    pub(crate) fn checkpoint_path(&self) -> &Path {
        &self.checkpoint_path
    }

    /// Merge drained projections. A new row takes ownership of the delta's
    /// charge; merging into an existing row releases it. Refused rows are
    /// released and counted, never partially merged.
    pub(crate) fn merge(&mut self, deltas: Vec<SnapshotDelta>) -> MergeOutcome {
        let mut outcome = MergeOutcome::default();
        for delta in deltas {
            let SnapshotDelta {
                key,
                window_end,
                value,
                charge,
            } = delta;
            let next_revision = self.next_revision.saturating_add(1);
            if let Some(row) = self.rows.get_mut(&key) {
                row.value = cumulative_add(row.value, value);
                row.window_end = row.window_end.max(window_end);
                row.revision = next_revision;
                self.next_revision = next_revision;
                outcome.updated += 1;
                outcome.released_bytes = outcome.released_bytes.saturating_add(charge);
                release_charge(charge);
                continue;
            }
            // The projection aggregator already admitted and charged this
            // identity. The snapshot store is a transfer target, not a second
            // admission gate: refusing here would drop an accepted projection.
            // Its live footprint remains bounded by METRIC_PROJECTION_POOL, and
            // an over-cap checkpoint turns into explicit backpressure when it
            // is persisted, not into a dropped row.
            let estimated = snapshot_row_cost(&key);
            let revision = self.bump_revision();
            // The creation revision doubles as the immutable incarnation. A
            // pruned row that is recreated later gets a fresh revision and
            // therefore a fresh incarnation/hash.
            let incarnation = revision;
            self.memory_bytes = self.memory_bytes.saturating_add(estimated);
            self.rows.insert(
                key.clone(),
                SnapshotRow {
                    key,
                    window_end,
                    value,
                    revision,
                    incarnation,
                    uploaded_revision: 0,
                    charge,
                    estimated_bytes: estimated,
                },
            );
            outcome.created += 1;
        }
        outcome
    }

    fn bump_revision(&mut self) -> u64 {
        self.next_revision = self.next_revision.saturating_add(1);
        self.next_revision
    }

    /// Dirty rows in deterministic identity order. The reporter groups them by
    /// wire header; the returned `revision` is this row's version.
    #[cfg(test)]
    pub(crate) fn dirty_rows(&self) -> Vec<DirtySnapshotRow> {
        self.dirty_rows_page(usize::MAX, u64::MAX)
    }

    /// Clone at most `limit` dirty rows and at most `max_bytes` estimated
    /// bytes. The reporter uploads one page at a time, so a node with a large
    /// backlog does not clone every row, build every protobuf record and hold
    /// them all at once. The first row is always included (it is then either
    /// quarantined or split), but later rows stop before the byte ceiling.
    pub(crate) fn dirty_rows_page(&self, limit: usize, max_bytes: u64) -> Vec<DirtySnapshotRow> {
        let mut page = Vec::new();
        let mut bytes = 0u64;
        for row in self
            .rows
            .values()
            .filter(|row| row.uploaded_revision < row.revision)
            .take(limit)
        {
            if !page.is_empty() && bytes.saturating_add(row.estimated_bytes) > max_bytes {
                break;
            }
            bytes = bytes.saturating_add(row.estimated_bytes);
            page.push(DirtySnapshotRow {
                key: row.key.clone(),
                window_end: row.window_end,
                value: row.value,
                revision: row.revision,
                incarnation: row.incarnation,
            });
        }
        page
    }

    /// Mark rows covered by a successfully durable upload. Sending a row at a
    /// higher request revision is safe because the payload always carries the
    /// current full value; a row edited after the snapshot was taken remains
    /// dirty because its own revision is higher.
    pub(crate) fn mark_uploaded(&mut self, rows: &[(SnapshotKey, u64)]) {
        for (key, uploaded) in rows {
            if let Some(row) = self.rows.get_mut(key) {
                row.uploaded_revision = row.uploaded_revision.max(*uploaded).min(row.revision);
            }
        }
    }

    /// Release rows as soon as their current revision has reached the durable
    /// spool or quarantine. The spool owns delivery until the remote ACK; this
    /// map need not keep another copy, even during the current minute. Further
    /// events for the same minute start a new incarnation that the receiver
    /// sums with the previous one. Dirty rows remain available for retry.
    pub(crate) fn release_uploaded(&mut self) -> usize {
        let mut released = 0;
        self.rows.retain(|_, row| {
            if row.uploaded_revision < row.revision {
                return true;
            }
            self.memory_bytes = self.memory_bytes.saturating_sub(row.estimated_bytes);
            release_charge(row.charge);
            released += 1;
            false
        });
        self.evicted_entries = self.evicted_entries.saturating_add(released as u64);
        released
    }

    pub(crate) fn quarantine_dir(&self) -> &Path {
        &self.quarantine_dir
    }

    /// Durably retain one record that cannot fit any wire chunk. This is the
    /// only "leaves the snapshot" path for a dirty row, and the file is never
    /// deleted by the reporter.
    pub(crate) async fn quarantine(
        &mut self,
        request: &pb::UploadMetricStatsRequest,
    ) -> anyhow::Result<u64> {
        let encoded = prost::Message::encode_to_vec(request);
        let dir = self.quarantine_dir.clone();
        let max_bytes = self.quarantine_max_bytes;
        let bytes = encoded.len() as u64;
        crate::worker_reclaim::spawn_storage_work(move || {
            persist_payload(&dir, "quarantine", Some(max_bytes), &encoded)
        })
        .await
        .map_err(|e| anyhow::anyhow!("metric stat quarantine task failed: {e}"))??;
        self.quarantined_records = self.quarantined_records.saturating_add(1);
        self.quarantined_bytes = self.quarantined_bytes.saturating_add(bytes);
        Ok(bytes)
    }

    pub(crate) fn record_retry_refusal(&mut self) {
        self.terminal_failed_uploads = self.terminal_failed_uploads.saturating_add(1);
    }

    pub(crate) fn set_backpressured(&mut self, backpressured: bool) {
        self.backpressured = backpressured;
    }

    pub(crate) fn dirty_len(&self) -> usize {
        self.rows
            .values()
            .filter(|row| row.uploaded_revision < row.revision)
            .count()
    }

    pub(crate) fn dirty_bytes(&self) -> u64 {
        self.rows
            .values()
            .filter(|row| row.uploaded_revision < row.revision)
            .map(|row| row.estimated_bytes)
            .fold(0u64, u64::saturating_add)
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn stats(&self) -> MetricValuesCacheStats {
        MetricValuesCacheStats {
            entries: self.len(),
            estimated_bytes: self.memory_bytes,
            max_entries: self.max_entries,
            max_estimated_bytes: self.max_estimated_bytes,
            dedup_max_estimated_bytes: self.max_estimated_bytes,
            max_pending_bytes: self.max_checkpoint_bytes,
            evicted_entries: self.evicted_entries,
            terminal_oversize_records: self.quarantined_records,
            terminal_failed_uploads: self.terminal_failed_uploads,
            pending_payloads: self.dirty_len(),
            pending_bytes: self.dirty_bytes(),
            backpressured: self.backpressured || self.checkpoint_error.is_some(),
            quarantined_bytes: self.quarantined_bytes,
        }
    }

    pub(crate) fn publish_stats(&self) -> MetricValuesCacheStats {
        let stats = self.stats();
        publish(stats);
        stats
    }

    #[cfg(test)]
    pub(crate) fn set_max_checkpoint_bytes(&mut self, max: u64) {
        self.max_checkpoint_bytes = max;
    }

    #[cfg(test)]
    pub(crate) fn row_value(&self, key: &SnapshotKey) -> Option<f32> {
        self.rows.get(key).map(|row| row.value)
    }

    #[cfg(test)]
    pub(crate) fn row_revision(&self, key: &SnapshotKey) -> Option<u64> {
        self.rows.get(key).map(|row| row.revision)
    }

    #[cfg(test)]
    pub(crate) fn row_incarnation(&self, key: &SnapshotKey) -> Option<u64> {
        self.rows.get(key).map(|row| row.incarnation)
    }
}

/// The issue34 branch used `metric_stat_quarantine/pending` for incremental
/// request payloads. Those deltas cannot be converted into cumulative snapshots
/// without guessing, so the files are left untouched and reported explicitly
/// instead of being silently ignored.
fn warn_legacy_quarantine(base_dir: &Path) {
    let Some(parent) = base_dir.parent() else {
        return;
    };
    let legacy = parent.join("metric_stat_quarantine");
    let Ok(bytes) = dir_usage(&legacy) else {
        return;
    };
    if bytes == 0 {
        return;
    }
    warn!(
        path = %legacy.display(),
        bytes,
        "legacy metric_stat_quarantine data left in place; it is not replayed because legacy incremental payloads cannot be converted into cumulative snapshots"
    );
}

impl Drop for MetricStatReporterState {
    fn drop(&mut self) {
        // The process is either exiting or the reporter is being replaced. The
        // checkpoint remains authoritative; release the pool charges the map
        // still owns so a retry/reopen starts from the file, not a leak.
        let bytes = self
            .rows
            .values()
            .map(|row| row.charge)
            .fold(0u64, u64::saturating_add);
        release_charge(bytes);
    }
}

impl Default for MetricStatReporterState {
    fn default() -> Self {
        Self::open(
            std::env::temp_dir().join(format!("zelvik-metric-snapshot-{}", uuid::Uuid::new_v4())),
        )
        .expect("create temporary metric snapshot state")
    }
}

fn cumulative_add(left: f32, right: f32) -> f32 {
    left + right
}

fn snapshot_key_heap_bytes(key: &SnapshotKey) -> u64 {
    let mut bytes = key.time_key.capacity() as u64;
    for part in &key.keys {
        bytes = bytes.saturating_add(part.capacity() as u64 + 16);
    }
    bytes
}

/// Actual live map cost: the BTreeMap owns a second copy of the key while the
/// row also stores its key, and both deep-copy their strings. The projection
/// charge reserves this larger form before the row reaches the snapshot store.
fn snapshot_row_cost(key: &SnapshotKey) -> u64 {
    let key_heap = snapshot_key_heap_bytes(key);
    let structs = (std::mem::size_of::<SnapshotRow>() + std::mem::size_of::<SnapshotKey>()) as u64;
    structs
        .saturating_add(key_heap.saturating_mul(2))
        .saturating_add(96)
}

fn release_charge(bytes: u64) {
    if bytes > 0 {
        crate::metrics::projection::METRIC_PROJECTION_POOL.refund(bytes);
    }
}

fn read_checkpoint_line(reader: &mut impl std::io::BufRead, max: u64) -> anyhow::Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        return Ok(Vec::new());
    }
    if line.len() as u64 > max {
        anyhow::bail!("metric snapshot checkpoint line exceeds the {max} byte cap");
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(line)
}

fn checkpoint_temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        let dir = std::fs::File::open(parent).map_err(|e| {
            anyhow::anyhow!(
                "open metric snapshot checkpoint directory {}: {e}",
                parent.display()
            )
        })?;
        dir.sync_all().map_err(|e| {
            anyhow::anyhow!(
                "sync metric snapshot checkpoint directory {}: {e}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

fn persist_payload(
    dir: &Path,
    prefix: &str,
    max_bytes: Option<u64>,
    encoded: &[u8],
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let bytes = encoded.len() as u64;
    if let Some(max_bytes) = max_bytes {
        let used = dir_usage(dir)?;
        if used.saturating_add(bytes) > max_bytes {
            anyhow::bail!(
                "metric stat quarantine is full: usedBytes={used} requiredBytes={bytes} maxBytes={max_bytes}"
            );
        }
    }
    let sequence = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let name = format!(
        "{prefix}-{sequence:020}-{}.metricstat.pb",
        uuid::Uuid::new_v4()
    );
    let final_path = dir.join(&name);
    let tmp_path = dir.join(format!(".{name}.tmp"));
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(encoded)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)?;
    let dir_file = std::fs::File::open(dir).map_err(|e| {
        anyhow::anyhow!(
            "open metric stat quarantine directory {}: {e}",
            dir.display()
        )
    })?;
    dir_file.sync_all().map_err(|e| {
        anyhow::anyhow!(
            "sync metric stat quarantine directory {}: {e}",
            dir.display()
        )
    })?;
    Ok(final_path)
}

/// Bytes currently held by files in `dir`. Missing directory means zero.
pub(crate) fn dir_usage(dir: &Path) -> std::io::Result<u64> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut total = 0u64;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(item_id: i64, keys: &[&str]) -> SnapshotKey {
        SnapshotKey {
            item_id,
            version: 1,
            time_key: "202610311200".into(),
            window_start: 1_790_000_000,
            period_end: 1_790_000_060,
            server_id: 7,
            keys: keys.iter().map(|key| (*key).to_string()).collect(),
        }
    }

    fn delta(key: SnapshotKey, value: f32) -> SnapshotDelta {
        SnapshotDelta {
            key,
            window_end: 1_790_000_060,
            value,
            charge: 0,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zelvik-metric-snapshot-test-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn cumulative_values_and_revision_are_monotonic_across_restart() {
        let dir = temp_dir("restart");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &["country=FR"]);
        let merged = state.merge(vec![delta(identity.clone(), 10.0)]);
        assert_eq!(merged.created, 1);
        let first_revision = state.row_revision(&identity).unwrap();
        state.merge(vec![delta(identity.clone(), 50.0)]);
        assert_eq!(state.row_value(&identity), Some(60.0));
        assert!(state.row_revision(&identity).unwrap() > first_revision);

        let rev = state.row_revision(&identity).unwrap();
        state.mark_uploaded(&[(identity.clone(), rev)]);
        assert_eq!(state.dirty_len(), 0);
        let bytes = state.encode_checkpoint().unwrap();
        write_atomic(&state.checkpoint_path.clone(), &bytes).unwrap();
        drop(state);

        let mut resumed = MetricStatReporterState::open(&dir).unwrap();
        assert_eq!(resumed.row_value(&identity), Some(60.0));
        assert_eq!(resumed.row_revision(&identity), Some(rev));
        assert_eq!(resumed.dirty_len(), 0);
        let merged = resumed.merge(vec![delta(identity.clone(), 5.0)]);
        assert_eq!(merged.updated, 1);
        assert!(resumed.row_revision(&identity).unwrap() > rev);
        assert_eq!(resumed.row_value(&identity), Some(65.0));
    }

    #[test]
    fn corrupt_checkpoint_is_an_explicit_error_and_is_not_replaced() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CHECKPOINT_NAME);
        std::fs::write(&path, b"{not json").unwrap();
        let err = match MetricStatReporterState::open(&dir) {
            Ok(_) => panic!("a corrupt checkpoint must not be treated as a fresh install"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("refusing to reset the epoch"));
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
    }

    #[test]
    fn revision_zero_checkpoint_is_rejected() {
        let dir = temp_dir("revision-zero");
        let state = MetricStatReporterState::open(&dir).unwrap();
        let mut bytes = serde_json::to_vec(&state.checkpoint_meta()).unwrap();
        bytes.push(b'\n');
        serde_json::to_writer(
            &mut bytes,
            &SnapshotRow {
                key: key(1, &[]),
                window_end: 1,
                value: 1.0,
                revision: 0,
                incarnation: 0,
                uploaded_revision: 0,
                charge: 0,
                estimated_bytes: 0,
            },
        )
        .unwrap();
        bytes.push(b'\n');
        std::fs::write(&state.checkpoint_path, bytes).unwrap();
        drop(state);
        let err = match MetricStatReporterState::open(&dir) {
            Ok(_) => panic!("a revision-0 checkpoint must be rejected"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("revision 0"));
    }

    #[test]
    fn new_epoch_is_created_only_when_no_checkpoint_exists() {
        let dir = temp_dir("fresh");
        let state = MetricStatReporterState::open(&dir).unwrap();
        assert!(!state.epoch().is_empty());
        assert!(state.checkpoint_path().exists());
        let epoch = state.epoch().to_string();
        drop(state);
        let reopened = MetricStatReporterState::open(&dir).unwrap();
        assert_eq!(reopened.epoch(), epoch);
    }

    #[test]
    fn legacy_and_snapshot_hashes_never_collide() {
        let identity = key(1, &["country=FR"]);
        let legacy = crate::utils::fnv_hash64(&format!(
            "{}@{}@{}@{}@{}",
            identity.server_id,
            identity.keys.join("$EDGE$"),
            identity.time_key,
            identity.version,
            identity.item_id
        ))
        .to_string();
        assert_ne!(identity.hash("epoch-a", 1), legacy);
        assert_ne!(identity.hash("epoch-a", 1), identity.hash("epoch-b", 1));
        assert_eq!(identity.hash("epoch-a", 1), identity.hash("epoch-a", 1));
        // The row-instance identity is part of the hash: same key, different
        // incarnation must never replace the earlier instance on the CP.
        assert_ne!(identity.hash("epoch-a", 1), identity.hash("epoch-a", 2));
    }

    #[test]
    fn durable_handoff_releases_even_the_current_minute() {
        let dir = temp_dir("release-current");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &["country=FR"]);
        state.merge(vec![delta(identity.clone(), 10.0)]);
        let revision = state.row_revision(&identity).unwrap();
        let hash = identity.hash(state.epoch(), state.row_incarnation(&identity).unwrap());
        state.mark_uploaded(&[(identity.clone(), revision)]);
        assert_eq!(state.release_uploaded(), 1);
        assert_eq!(state.stats().entries, 0);
        assert_eq!(state.stats().estimated_bytes, 0);
        assert_eq!(state.stats().pending_bytes, 0);

        state.merge(vec![delta(identity.clone(), 5.0)]);
        assert_eq!(state.row_value(&identity), Some(5.0));
        assert_ne!(
            identity.hash(state.epoch(), state.row_incarnation(&identity).unwrap()),
            hash
        );
    }

    #[test]
    fn acknowledging_an_older_revision_keeps_newer_data_dirty() {
        let dir = temp_dir("older-upload");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &["country=FR"]);
        state.merge(vec![delta(identity.clone(), 10.0)]);
        let uploaded_revision = state.row_revision(&identity).unwrap();
        state.merge(vec![delta(identity.clone(), 5.0)]);
        state.mark_uploaded(&[(identity.clone(), uploaded_revision)]);
        assert_eq!(state.release_uploaded(), 0);
        assert_eq!(state.dirty_len(), 1);
        assert_eq!(state.row_value(&identity), Some(15.0));
    }

    #[test]
    fn pruned_row_recreated_by_a_late_delta_gets_a_new_hash() {
        let dir = temp_dir("reincarnation");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &["country=FR"]);

        // The first instance reaches 10 and transfers to the durable spool.
        // Later events must add under a new hash, never replace those 10.
        state.merge(vec![delta(identity.clone(), 10.0)]);
        let old_revision = state.row_revision(&identity).unwrap();
        let old_incarnation = state.row_incarnation(&identity).unwrap();
        state.mark_uploaded(&[(identity.clone(), old_revision)]);
        assert_eq!(state.release_uploaded(), 1);
        assert_eq!(state.len(), 0);

        // A late completion for the same period recreates the cumulative row
        // from zero, but as a *new* instance with its own hash.
        state.merge(vec![delta(identity.clone(), 5.0)]);
        let new_incarnation = state.row_incarnation(&identity).unwrap();
        assert_ne!(new_incarnation, old_incarnation);
        assert_eq!(state.row_value(&identity), Some(5.0));
        assert_ne!(
            identity.hash(state.epoch(), old_incarnation),
            identity.hash(state.epoch(), new_incarnation)
        );

        // Further deltas for the same instance keep the hash stable.
        let new_hash = identity.hash(state.epoch(), new_incarnation);
        state.merge(vec![delta(identity.clone(), 2.0)]);
        assert_eq!(state.row_value(&identity), Some(7.0));
        assert_eq!(state.row_incarnation(&identity), Some(new_incarnation));
        assert_eq!(identity.hash(state.epoch(), new_incarnation), new_hash);
    }

    #[test]
    fn incarnation_survives_checkpoint_restart() {
        let dir = temp_dir("incarnation-restart");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &["country=FR"]);
        state.merge(vec![delta(identity.clone(), 5.0)]);
        let incarnation = state.row_incarnation(&identity).unwrap();
        let hash = identity.hash(state.epoch(), incarnation);

        let bytes = state.encode_checkpoint().unwrap();
        write_atomic(&state.checkpoint_path.clone(), &bytes).unwrap();
        let epoch = state.epoch().to_string();
        drop(state);

        let mut resumed = MetricStatReporterState::open(&dir).unwrap();
        assert_eq!(resumed.epoch(), epoch);
        assert_eq!(resumed.row_incarnation(&identity), Some(incarnation));
        assert_eq!(identity.hash(resumed.epoch(), incarnation), hash);

        // A later delta on the recovered instance still uploads the same hash.
        resumed.merge(vec![delta(identity.clone(), 1.0)]);
        assert_eq!(resumed.row_value(&identity), Some(6.0));
        assert_eq!(resumed.row_incarnation(&identity), Some(incarnation));
        assert_eq!(identity.hash(resumed.epoch(), incarnation), hash);
    }

    #[test]
    fn dirty_rows_survive_release_but_uploaded_rows_do_not() {
        let dir = temp_dir("release");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let identity = key(1, &[]);
        state.merge(vec![delta(identity.clone(), 1.0)]);
        assert_eq!(state.release_uploaded(), 0);
        let rev = state.row_revision(&identity).unwrap();
        state.mark_uploaded(&[(identity.clone(), rev)]);
        assert_eq!(state.release_uploaded(), 1);
        assert_eq!(state.len(), 0);
    }

    #[tokio::test]
    async fn repeated_handoffs_and_restarts_do_not_retain_history_or_reuse_hashes() {
        let dir = temp_dir("fixed-dimensions");
        let mut state = MetricStatReporterState::open(&dir).unwrap();
        let mut hashes = std::collections::BTreeSet::new();
        for minute in 0..120 {
            let mut identity = key(1, &["country=FR"]);
            identity.window_start += minute * 60;
            identity.period_end += minute * 60;
            identity.time_key = minute.to_string();
            // Two handoffs in one minute and a restart after each. Repeating
            // the same dimension never creates retained historical counters.
            for _ in 0..2 {
                state.merge(vec![delta(identity.clone(), 1.0)]);
                let revision = state.row_revision(&identity).unwrap();
                assert!(hashes.insert(
                    identity.hash(state.epoch(), state.row_incarnation(&identity).unwrap())
                ));
                state.persist_checkpoint().await.unwrap();
                state.mark_uploaded(&[(identity.clone(), revision)]);
                assert_eq!(state.release_uploaded(), 1);
                state.persist_checkpoint().await.unwrap();
                assert_eq!(state.stats().estimated_bytes, 0);
                drop(state);
                state = MetricStatReporterState::open(&dir).unwrap();
                assert_eq!(state.len(), 0);
                assert_eq!(state.release_uploaded(), 0);
            }
        }
        drop(state);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
