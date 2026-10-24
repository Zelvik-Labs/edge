//! Bounded, window-scoped state for the `metric_stat` reporter.
//!
//! The reporter may skip a hash whose value did not change since the previous
//! upload and send it as `keep_keys` instead of `metric_stats`. That
//! optimization is only valid while the receiver still holds the previous
//! value, so an entry must be scoped to the item's current configuration
//! version and reporting window, committed only after the payload is durable,
//! and bounded in both entry count and estimated bytes.
//!
//! Evicting an entry is always safe: the next upload for that hash simply
//! carries the full `metric_stats` row again. Eviction may cost bandwidth, but
//! it never drops a sample.
//!
//! Payloads that cannot reach the spool are never dropped either. They wait in
//! per-item queues (so one failing item cannot gate the others) and are fsynced
//! to a durable local pending directory before they leave the aggregator's
//! logical ownership; a restart reloads them. Oversized records are fsynced to
//! a durable quarantine directory instead. The only global control is lossless
//! backpressure: new samples stay in the aggregator until a later tick flushes
//! them, and while a retry queue exists a tick that would exceed the remaining
//! pending budget is deferred before flushing. A spool failure during a tick
//! can still retain that one tick's payloads atomically; they are bounded by
//! the size of one aggregation, and further flushes are blocked until the queue
//! drains below the ceiling.

use lru::LruCache;
use parking_lot::RwLock;
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tracing::warn;

use crate::pb;

/// Upper bound on retained dedup entries for a typical node. A node with more
/// distinct (server, dimension, window, version, item) tuples than this keeps
/// reporting correctly; it only loses the unchanged-value optimization for
/// the least recently used keys.
pub(crate) const DEFAULT_MAX_ENTRIES: usize = 100_000;
/// Upper bound on the estimated heap+struct footprint of the dedup cache.
pub(crate) const DEFAULT_MAX_ESTIMATED_BYTES: u64 = 32 * 1024 * 1024;
/// Retained payload bytes above which the reporter stops taking new samples
/// out of the aggregator. Backpressure is never a drop: the samples stay in
/// the aggregator and the already-built payloads keep retrying.
pub(crate) const DEFAULT_MAX_PENDING_BYTES: u64 = 64 * 1024 * 1024;
/// On-disk ceiling for records that cannot fit the wire at all. Files in this
/// directory are never deleted by the reporter; when the ceiling is reached
/// the payload stays in the per-item retry queue (no drop).
pub(crate) const DEFAULT_MAX_QUARANTINE_BYTES: u64 = 256 * 1024 * 1024;

/// Observable size of the metric values cache, also consumed by memory
/// governance. Every value is a real observation; nothing here is estimated
/// from a different counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricValuesCacheStats {
    /// Entries currently retained for the active windows.
    pub entries: usize,
    /// Estimated bytes held by the dedup entries plus every in-memory pending
    /// payload. Durable quarantine bytes live on disk and are reported
    /// separately in `quarantined_bytes`.
    pub estimated_bytes: u64,
    /// Configured entry ceiling.
    pub max_entries: usize,
    /// Combined ceiling: dedup-cache limit plus pending-payload limit.
    pub max_estimated_bytes: u64,
    /// Dedup-cache-only estimate ceiling, so the two budgets stay separable.
    pub dedup_max_estimated_bytes: u64,
    /// Pending-payload backpressure ceiling.
    pub max_pending_bytes: u64,
    /// Cumulative entries dropped because a ceiling was reached. Eviction
    /// only widens the next upload; it never drops the sample itself.
    pub evicted_entries: u64,
    /// Records that could not fit any wire-sized upload even alone and were
    /// written to the durable local quarantine directory (full payload).
    /// Deliberately not deleted; no field here represents a dropped record.
    pub terminal_oversize_records: u64,
    /// Non-capacity spool refusals observed and retained for another attempt.
    /// The payload still sits at the head of its item queue; this is a
    /// diagnostic counter, not a drop counter.
    pub terminal_failed_uploads: u64,
    /// Payloads waiting in per-item retry queues (spool or quarantine).
    pub pending_payloads: usize,
    /// Estimated encoded bytes retained by those payloads.
    pub pending_bytes: u64,
    /// True while retained bytes reached the backpressure ceiling and new
    /// samples are intentionally left in the aggregator.
    pub backpressured: bool,
    /// Bytes written to the durable quarantine directory.
    pub quarantined_bytes: u64,
}

static PUBLISHED_STATS: LazyLock<RwLock<MetricValuesCacheStats>> =
    LazyLock::new(|| RwLock::new(MetricValuesCacheStats::default()));

/// Latest cache size published by the reporter, readable without touching the
/// reporter task. Returns zeroes before the reporter has completed a tick.
pub fn metric_values_cache_stats() -> MetricValuesCacheStats {
    *PUBLISHED_STATS.read()
}

fn publish(stats: MetricValuesCacheStats) {
    *PUBLISHED_STATS.write() = stats;
}

/// The current config version and reporting window of an enabled item.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LiveWindow {
    version: i32,
    time_key: Box<str>,
}

/// Config lifecycle view for one reporter tick: only enabled items are live,
/// and each points at exactly one window.
#[derive(Clone, Debug, Default)]
pub(crate) struct MetricScope {
    live: HashMap<i64, LiveWindow>,
}

impl MetricScope {
    pub(crate) fn from_windows<I, S>(windows: I) -> Self
    where
        I: IntoIterator<Item = (i64, i32, S)>,
        S: Into<Box<str>>,
    {
        Self {
            live: windows
                .into_iter()
                .map(|(item_id, version, time_key)| {
                    (
                        item_id,
                        LiveWindow {
                            version,
                            time_key: time_key.into(),
                        },
                    )
                })
                .collect(),
        }
    }

    fn matches(&self, item_id: i64, version: i32, time_key: &str) -> bool {
        self.live
            .get(&item_id)
            .is_some_and(|window| window.version == version && window.time_key.as_ref() == time_key)
    }
}

struct CachedValue {
    item_id: i64,
    version: i32,
    time_key: Box<str>,
    value: f32,
    cost: u64,
}

/// Dedup state for unchanged metric values.
///
/// All keys are scoped by item id (inside the key), configuration version and
/// reporting window (inside the entry), so a disabled/deleted item, a version
/// bump or a new window makes every old entry stale and removable.
pub(crate) struct MetricValuesCache {
    entries: LruCache<String, CachedValue>,
    estimated_bytes: u64,
    evicted_entries: u64,
    max_entries: usize,
    max_estimated_bytes: u64,
}

impl MetricValuesCache {
    pub(crate) fn new(max_entries: usize, max_estimated_bytes: u64) -> Self {
        let max_entries = max_entries.max(1);
        Self {
            entries: LruCache::new(NonZeroUsize::new(max_entries).unwrap_or(NonZeroUsize::MIN)),
            estimated_bytes: 0,
            evicted_entries: 0,
            max_entries,
            max_estimated_bytes,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }

    /// Previous value for this hash, but only while it belongs to the same
    /// configuration version and window. A stale entry is treated as absent:
    /// the caller sends the full row.
    pub(crate) fn lookup(
        &mut self,
        item_id: i64,
        version: i32,
        time_key: &str,
        hash: &str,
    ) -> Option<f32> {
        let key = cache_key(item_id, hash);
        self.entries
            .get(&key)
            .filter(|entry| entry.version == version && entry.time_key.as_ref() == time_key)
            .map(|entry| entry.value)
    }

    /// Record a value that is now durable. Returns false when the item/window
    /// is no longer current, in which case the commit is deliberately skipped:
    /// a retry of an old window must not seed the cache for the live one.
    pub(crate) fn commit(
        &mut self,
        scope: &MetricScope,
        item_id: i64,
        version: i32,
        time_key: &str,
        hash: &str,
        value: f32,
    ) -> bool {
        if !scope.matches(item_id, version, time_key) {
            return false;
        }

        let key = cache_key(item_id, hash);
        let cost = entry_cost(&key, time_key);
        let replacing = self.entries.contains(&key);
        if !replacing && self.entries.len() >= self.max_entries {
            self.evict_lru();
        }
        if let Some(old) = self.entries.put(
            key,
            CachedValue {
                item_id,
                version,
                time_key: time_key.into(),
                value,
                cost,
            },
        ) {
            self.estimated_bytes = self.estimated_bytes.saturating_sub(old.cost);
        }
        self.estimated_bytes = self.estimated_bytes.saturating_add(cost);
        while self.estimated_bytes > self.max_estimated_bytes {
            if !self.evict_lru() {
                break;
            }
        }
        true
    }

    fn evict_lru(&mut self) -> bool {
        let Some((_, evicted)) = self.entries.pop_lru() else {
            return false;
        };
        self.estimated_bytes = self.estimated_bytes.saturating_sub(evicted.cost);
        self.evicted_entries = self.evicted_entries.saturating_add(1);
        true
    }

    /// Drop every entry that is not the current config version/window of a
    /// live item. Runs on every reporter tick, even when there are no items or
    /// no samples, so an idle node cannot retain historical windows.
    pub(crate) fn prune(&mut self, scope: &MetricScope) -> usize {
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                !scope.matches(entry.item_id, entry.version, entry.time_key.as_ref())
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &stale {
            if let Some(entry) = self.entries.pop(key.as_str()) {
                self.estimated_bytes = self.estimated_bytes.saturating_sub(entry.cost);
            }
        }
        stale.len()
    }

    pub(crate) fn stats(&self) -> MetricValuesCacheStats {
        MetricValuesCacheStats {
            entries: self.len(),
            estimated_bytes: self.estimated_bytes(),
            max_entries: self.max_entries,
            max_estimated_bytes: self.max_estimated_bytes,
            dedup_max_estimated_bytes: self.max_estimated_bytes,
            max_pending_bytes: 0,
            evicted_entries: self.evicted_entries,
            terminal_oversize_records: 0,
            terminal_failed_uploads: 0,
            pending_payloads: 0,
            pending_bytes: 0,
            backpressured: false,
            quarantined_bytes: 0,
        }
    }
}

impl Default for MetricValuesCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_ESTIMATED_BYTES)
    }
}

fn cache_key(item_id: i64, hash: &str) -> String {
    format!("{item_id}_{hash}")
}

fn entry_cost(key: &str, time_key: &str) -> u64 {
    (std::mem::size_of::<CachedValue>() + key.len() + time_key.len()) as u64
}

/// One already-built payload waiting for either the durable spool or the
/// durable local quarantine. The bytes are never rebuilt: an attempt sends or
/// stores exactly this request. `quarantine_only` marks a record that cannot
/// fit the wire at all, so it must never be offered to the spool.
///
/// `durable_path` is set once the exact request has been fsynced to the local
/// pending directory. It is what makes a restart recover the payload instead
/// of losing data that already left the aggregator.
pub(crate) struct PendingMetricUpload {
    pub(crate) request: pb::UploadMetricStatsRequest,
    pub(crate) encoded_bytes: u64,
    pub(crate) quarantine_only: bool,
    pub(crate) durable_path: Option<PathBuf>,
}

impl PendingMetricUpload {
    pub(crate) fn for_spool(request: pb::UploadMetricStatsRequest) -> Self {
        Self {
            encoded_bytes: prost::Message::encoded_len(&request) as u64,
            request,
            quarantine_only: false,
            durable_path: None,
        }
    }

    pub(crate) fn for_quarantine(request: pb::UploadMetricStatsRequest) -> Self {
        Self {
            encoded_bytes: prost::Message::encoded_len(&request) as u64,
            request,
            quarantine_only: true,
            durable_path: None,
        }
    }
}

/// Durable local store for `metric_stat` payloads that cannot reach the spool
/// yet.
///
/// Layout under the base directory:
/// - `quarantine/`: records the wire cannot carry even alone. Written once,
///   never replayed or deleted by the reporter; they survive restart for an
///   operator/replay path and are counted in `quarantined_bytes`.
/// - `pending/`: exact requests waiting for spool capacity or a transient
///   failure. Written before the payload is removed from the aggregator's
///   logical ownership, replayed into per-item queues on reporter start, and
///   deleted only after the spool accepts the exact bytes (a crash between
///   accept and delete replays an idempotent `metric_stat` row, not new data).
///
/// Every file is written to a temporary name, fsynced, renamed, and followed
/// by a directory fsync, so a crash cannot leave a truncated file that looks
/// durable.
pub(crate) struct DiskMetricQuarantine {
    quarantine_dir: PathBuf,
    pending_dir: PathBuf,
    max_bytes: u64,
}

impl DiskMetricQuarantine {
    pub(crate) fn new(base_dir: impl Into<PathBuf>, max_bytes: u64) -> Self {
        let base_dir = base_dir.into();
        Self {
            quarantine_dir: base_dir.join("quarantine"),
            pending_dir: base_dir.join("pending"),
            max_bytes,
        }
    }

    pub(crate) fn quarantine_dir(&self) -> &Path {
        &self.quarantine_dir
    }

    pub(crate) fn pending_dir(&self) -> &Path {
        &self.pending_dir
    }

    #[cfg(test)]
    pub(crate) fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
    }

    /// Persist the complete request before returning `Ok`, so a successful
    /// return is a durable quarantine and never just an in-memory marker.
    pub(crate) async fn store(
        &self,
        request: &pb::UploadMetricStatsRequest,
    ) -> anyhow::Result<u64> {
        let encoded = request.encode_to_vec();
        let bytes = encoded.len() as u64;
        let dir = self.quarantine_dir.clone();
        let max_bytes = Some(self.max_bytes);
        tokio::task::spawn_blocking(move || {
            persist_payload(&dir, "quarantine", max_bytes, &encoded)
        })
        .await
        .map_err(|e| anyhow::anyhow!("metric stat quarantine task failed: {e}"))??;
        Ok(bytes)
    }

    /// Fsync the exact request into `pending/` and return its durable path.
    pub(crate) async fn store_pending(
        &self,
        request: &pb::UploadMetricStatsRequest,
    ) -> anyhow::Result<PathBuf> {
        let encoded = request.encode_to_vec();
        let dir = self.pending_dir.clone();
        tokio::task::spawn_blocking(move || persist_payload(&dir, "pending", None, &encoded))
            .await
            .map_err(|e| anyhow::anyhow!("metric stat pending task failed: {e}"))?
    }

    /// Recover exact requests left in `pending/` by a previous process. Files
    /// are returned in write order so per-item order is preserved after a
    /// restart. Unreadable files are left in place and reported, never deleted.
    pub(crate) fn recover_pending(
        &self,
    ) -> Vec<anyhow::Result<(PathBuf, pb::UploadMetricStatsRequest)>> {
        let mut paths = Vec::new();
        let entries = match std::fs::read_dir(&self.pending_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                return vec![Err(anyhow::anyhow!(
                    "read metric stat pending dir {}: {error}",
                    self.pending_dir.display()
                ))];
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("pb") {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path)
                    .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
                let request =
                    <pb::UploadMetricStatsRequest as prost::Message>::decode(bytes.as_slice())
                        .map_err(|e| anyhow::anyhow!("decode {}: {e}", path.display()))?;
                Ok((path, request))
            })
            .collect()
    }

    pub(crate) fn remove_pending(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_file(path)
    }
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
    // The timestamp prefix keeps write order after a restart; the UUID keeps
    // names unique. Sync the temporary file before the rename and the
    // directory after it, so a crash exposes either no file or a complete one.
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
    if let Ok(dir_file) = std::fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
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

/// Reporter-owned dedup cache plus the exact payloads waiting for a durable
/// destination.
///
/// Pending payloads are partitioned by item: one item whose spool or
/// quarantine is permanently failing stalls only its own queue, while every
/// other item continues to upload. A newer payload for an item whose queue is
/// non-empty is appended behind the older one, so per-item order is preserved.
/// No code path removes a payload except a successful durable write.
pub(crate) struct MetricStatReporterState {
    cache: MetricValuesCache,
    pending: HashMap<i64, VecDeque<PendingMetricUpload>>,
    pending_bytes: u64,
    max_pending_bytes: u64,
    terminal_failed_uploads: u64,
    quarantined_records: u64,
    quarantined_bytes: u64,
    backpressured: bool,
    quarantine: DiskMetricQuarantine,
}

impl MetricStatReporterState {
    pub(crate) fn new(max_entries: usize, max_estimated_bytes: u64) -> Self {
        Self::with_quarantine(
            max_entries,
            max_estimated_bytes,
            DEFAULT_MAX_PENDING_BYTES,
            std::env::temp_dir().join(format!("zelvik-metric-quarantine-{}", uuid::Uuid::new_v4())),
        )
    }

    /// Production constructor: default cache/backpressure limits with an
    /// explicit durable quarantine directory under the node's data dir.
    pub(crate) fn with_quarantine_dir(quarantine_dir: impl Into<PathBuf>) -> Self {
        Self::with_quarantine(
            DEFAULT_MAX_ENTRIES,
            DEFAULT_MAX_ESTIMATED_BYTES,
            DEFAULT_MAX_PENDING_BYTES,
            quarantine_dir,
        )
    }

    pub(crate) fn with_quarantine(
        max_entries: usize,
        max_estimated_bytes: u64,
        max_pending_bytes: u64,
        quarantine_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            cache: MetricValuesCache::new(max_entries, max_estimated_bytes),
            pending: HashMap::new(),
            pending_bytes: 0,
            max_pending_bytes,
            terminal_failed_uploads: 0,
            quarantined_records: 0,
            quarantined_bytes: 0,
            backpressured: false,
            quarantine: DiskMetricQuarantine::new(quarantine_dir, DEFAULT_MAX_QUARANTINE_BYTES),
        }
    }

    pub(crate) fn cache_mut(&mut self) -> &mut MetricValuesCache {
        &mut self.cache
    }

    pub(crate) fn prune(&mut self, scope: &MetricScope) -> usize {
        self.cache.prune(scope)
    }

    pub(crate) fn stats(&self) -> MetricValuesCacheStats {
        let cache = self.cache.stats();
        MetricValuesCacheStats {
            terminal_oversize_records: self.quarantined_records,
            terminal_failed_uploads: self.terminal_failed_uploads,
            pending_payloads: self.pending_len(),
            pending_bytes: self.pending_bytes,
            backpressured: self.backpressured,
            quarantined_bytes: self.quarantined_bytes,
            // Memory governance consumes `estimated_bytes`; pending payloads
            // are real RAM and must be visible there, not only the dedup cache.
            estimated_bytes: cache.estimated_bytes.saturating_add(self.pending_bytes),
            // The estimate covers two separately bounded pools, so the ceiling
            // is their sum. The dedup-only ceiling stays visible separately.
            max_estimated_bytes: cache
                .max_estimated_bytes
                .saturating_add(self.max_pending_bytes),
            dedup_max_estimated_bytes: cache.max_estimated_bytes,
            max_pending_bytes: self.max_pending_bytes,
            ..cache
        }
    }

    pub(crate) fn publish_stats(&self) -> MetricValuesCacheStats {
        let stats = self.stats();
        publish(stats);
        stats
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.values().map(VecDeque::len).sum()
    }

    pub(crate) fn pending_bytes(&self) -> u64 {
        self.pending_bytes
    }

    pub(crate) fn max_pending_bytes(&self) -> u64 {
        self.max_pending_bytes
    }

    pub(crate) fn has_pending(&self, item_id: i64) -> bool {
        self.pending
            .get(&item_id)
            .is_some_and(|queue| !queue.is_empty())
    }

    pub(crate) fn pending_item_ids(&self) -> Vec<i64> {
        self.pending
            .iter()
            .filter(|(_, queue)| !queue.is_empty())
            .map(|(item_id, _)| *item_id)
            .collect()
    }

    /// Append behind anything already queued for this item, preserving order.
    pub(crate) fn push_pending(&mut self, item_id: i64, pending: PendingMetricUpload) {
        self.pending_bytes = self.pending_bytes.saturating_add(pending.encoded_bytes);
        self.pending.entry(item_id).or_default().push_back(pending);
    }

    pub(crate) fn pop_pending(&mut self, item_id: i64) -> Option<PendingMetricUpload> {
        let pending = self.pending.get_mut(&item_id)?.pop_front()?;
        self.pending_bytes = self.pending_bytes.saturating_sub(pending.encoded_bytes);
        Some(pending)
    }

    /// Put a failed attempt back at the head: no other payload for this item
    /// may overtake it.
    pub(crate) fn return_pending_front(&mut self, item_id: i64, pending: PendingMetricUpload) {
        self.pending_bytes = self.pending_bytes.saturating_add(pending.encoded_bytes);
        self.pending.entry(item_id).or_default().push_front(pending);
    }

    pub(crate) fn record_retry_refusal(&mut self) {
        self.terminal_failed_uploads = self.terminal_failed_uploads.saturating_add(1);
    }

    pub(crate) fn set_backpressured(&mut self, backpressured: bool) {
        self.backpressured = backpressured;
    }

    pub(crate) fn quarantine_dir(&self) -> &Path {
        self.quarantine.quarantine_dir()
    }

    pub(crate) fn pending_dir(&self) -> &Path {
        self.quarantine.pending_dir()
    }

    #[cfg(test)]
    pub(crate) fn set_quarantine_max_bytes(&mut self, max_bytes: u64) {
        self.quarantine.set_max_bytes(max_bytes);
    }

    /// Fsync a payload before it is left waiting for the spool, so a restart
    /// can recover it. On failure the payload is still kept in memory: this is
    /// a best-effort durability upgrade, never a drop path.
    pub(crate) async fn persist_pending(
        &mut self,
        pending: PendingMetricUpload,
    ) -> PendingMetricUpload {
        if pending.durable_path.is_some() {
            return pending;
        }
        match self.quarantine.store_pending(&pending.request).await {
            Ok(path) => PendingMetricUpload {
                durable_path: Some(path),
                ..pending
            },
            Err(e) => {
                warn!(
                    item_id = pending.request.item_id,
                    bytes = pending.encoded_bytes,
                    "could not fsync metric stat pending payload; retaining in memory: {e}"
                );
                pending
            }
        }
    }

    /// Remove the durable copy only after the exact bytes reached the spool or
    /// the quarantine. A crash before this delete replays an idempotent row,
    /// never new data.
    pub(crate) fn discard_pending_file(&self, pending: &PendingMetricUpload) {
        let Some(path) = &pending.durable_path else {
            return;
        };
        if let Err(e) = self.quarantine.remove_pending(path) {
            warn!(
                path = %path.display(),
                "could not remove a delivered metric stat pending file (replay is idempotent): {e}"
            );
        }
    }

    /// Reload exact pending requests left by a previous process. Filenames are
    /// timestamp-prefixed, so per-item order is preserved.
    pub(crate) fn recover_pending(&mut self) -> usize {
        let mut recovered = 0;
        for result in self.quarantine.recover_pending() {
            match result {
                Ok((path, request)) => {
                    let item_id = request.item_id;
                    self.push_pending(
                        item_id,
                        PendingMetricUpload {
                            encoded_bytes: prost::Message::encoded_len(&request) as u64,
                            request,
                            quarantine_only: false,
                            durable_path: Some(path),
                        },
                    );
                    recovered += 1;
                }
                Err(e) => warn!(
                    "could not recover a metric stat pending payload; file left in place: {e}"
                ),
            }
        }
        recovered
    }

    /// Durably store one record that cannot fit the wire. Only a successful
    /// durable write removes it from the caller's queue and increments the
    /// quarantine counters.
    pub(crate) async fn store_quarantine(
        &mut self,
        request: &pb::UploadMetricStatsRequest,
    ) -> anyhow::Result<u64> {
        let bytes = self.quarantine.store(request).await?;
        self.quarantined_records = self.quarantined_records.saturating_add(1);
        self.quarantined_bytes = self.quarantined_bytes.saturating_add(bytes);
        Ok(bytes)
    }
}

impl Default for MetricStatReporterState {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_MAX_ESTIMATED_BYTES)
    }
}
