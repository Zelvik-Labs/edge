//! Durable producer queues. API memory is not a delivery acknowledgement.
use crate::{
    api_config::ApiConfig,
    paths::NodePaths,
    pb,
    rpc::client::{RPC_NODE_STREAM_MESSAGE_BYTES, SharedRpcClient},
};
use mace::{Bucket, BucketOptions, Mace, OpCode, Options, TxnKV};
use parking_lot::Mutex;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};
use tracing::{debug, error, warn};
use zelvik_suite::controlerr::Code as ControlCode;
use zelvik_suite::{TRAILER_ERROR, TRAILER_RETRYABLE};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub assignment_version: String,
    pub route_version: String,
    pub cluster_id: String,
    pub billing_region_id: String,
}
static BINDING: std::sync::LazyLock<parking_lot::RwLock<Binding>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(Binding::default()));
/// Process-boot identity splits per-window statistic segments: a queue boot is
/// stable across restarts (receipt dedup) while a process boot changes, so a
/// post-restart segment can never overwrite rows produced before the restart.
static PROCESS_BOOT: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());
pub fn process_boot() -> &'static str {
    &PROCESS_BOOT
}
pub fn set_binding(binding: Binding) {
    *BINDING.write() = binding;
}
pub fn binding() -> Binding {
    BINDING.read().clone()
}

#[derive(Clone, Serialize, Deserialize)]
struct Meta {
    head: u64,
    tail: u64,
    bytes: u64,
    boot: String,
    #[serde(default)]
    oldest_at: i64,
    #[serde(default)]
    meter_started: i64,
    #[serde(default)]
    meter_bytes: u64,
    #[serde(default)]
    acked_ahead: u64,
    #[serde(default)]
    acked_ahead_bytes: u64,
    /// Durable loss accounting. Every field below is written in the same Mace
    /// transaction that removes the payload it describes, so a crash can never
    /// leave a loss uncounted, unreasoned or unreported.
    #[serde(default)]
    quarantined: u64,
    #[serde(default)]
    quarantined_bytes: u64,
    #[serde(default)]
    retryable_failures: u64,
    #[serde(default)]
    terminal_failures: u64,
    #[serde(default)]
    oversize_dropped: u64,
    /// Id of the next durable loss record (see [`LossRecord`]).
    #[serde(default)]
    journal_id: u64,
    /// Reason of the most recent loss, restored on reopen.
    #[serde(default)]
    last_loss_reason: String,
    /// This lane's own gap bounds: the durable half of the process-wide
    /// `GAP_START`/`GAP_END`/`GAP_CORE`, merged back into the statics on open.
    #[serde(default)]
    gap_start: i64,
    #[serde(default)]
    gap_end: i64,
    #[serde(default)]
    gap_has_core: bool,
}
impl Default for Meta {
    fn default() -> Self {
        Self {
            head: 1,
            tail: 1,
            bytes: 0,
            boot: uuid::Uuid::new_v4().to_string(),
            oldest_at: 0,
            meter_started: 0,
            meter_bytes: 0,
            acked_ahead: 0,
            acked_ahead_bytes: 0,
            quarantined: 0,
            quarantined_bytes: 0,
            retryable_failures: 0,
            terminal_failures: 0,
            oversize_dropped: 0,
            journal_id: 0,
            last_loss_reason: String::new(),
            gap_start: 0,
            gap_end: 0,
            gap_has_core: false,
        }
    }
}
pub struct Spool {
    _db: Mace,
    bucket: Bucket,
    meta: Mutex<Meta>,
    path: PathBuf,
    budget: LaneBudget,
    /// Durable loss accounting, mirrored from [`Meta`]: batches dropped after a
    /// permanent rejection, the per-class upload failure counters, single
    /// records too large to batch, and the reason of the most recent loss. Kept
    /// per lane so a status reader can tell which lane lost data and why, and
    /// restored from the durable meta on open so a restart does not forget it.
    quarantined: AtomicU64,
    retryable_failures: AtomicU64,
    terminal_failures: AtomicU64,
    oversize_dropped: AtomicU64,
    last_loss_reason: parking_lot::RwLock<String>,
    #[cfg(test)]
    fixed_disk: Option<(u64, u64)>,
}
/// Capacity policy of the durable spool.
///
/// The reserve is *adaptive*: a fixed ratio of the disk's total size caps it, a
/// floor keeps a minimum tail free, and on a tight disk the floor yields to a
/// quarter of what is actually available. Applying the ratio as a cap is what
/// stops a large `total` from reserving more than the disk has free — the old
/// `max(total / 50, 1 GiB)` did exactly that and turned a nearly-full disk into
/// "capacity 0 for every lane".
///
/// Consequence worth stating plainly: on a filesystem smaller than
/// `RESERVE_FLOOR * RESERVE_RATIO` (3.2 GiB) the 1/50 share is below the floor,
/// so the reserve is proportional rather than a full 64 MiB. That keeps a small
/// disk usable instead of reserving a quarter of it, at the cost of a smaller
/// absolute tail than `RESERVE_FLOOR` promises on large disks.
///
/// From that free budget `core` additionally holds back a slice for the detail
/// lane, and both lanes hold back whatever the *other* lane has already written
/// but not acknowledged. The reservation is one-sided on purpose: core is
/// charged for detail so billing batches cannot spend the detail slice, while
/// detail is not charged for core because a fresh filesystem reading already
/// reflects core's bytes and detail is the fallback lane that lives in whatever
/// core leaves. Charging both lanes for each other would let the two budgets
/// sum past the free space, which is what the original `core_reserve` comment
/// intended but the code (reserving from the wrong lane) never achieved.
/// 同时在途的上传批次上限。批次本身已按秒合并，8 路足以吃掉 RTT，
/// 又不至于在断网恢复瞬间把节点和 API 一起打满。
const REPLAY_CONCURRENCY: usize = 8;
const RESERVE_FLOOR: u64 = 64 << 20;
const RESERVE_RATIO: u64 = 50;
/// At most a quarter of the *available* space may be reserved, so a lane's
/// capacity never drops below 3/4 of what is free (and is 0 only when the disk
/// is actually full).
const RESERVE_AVAILABLE_DIVISOR: u64 = 4;
/// Slice of the free budget held back from `core` so that quality observations
/// (and other detail telemetry) keep a place to land while billing batches are
/// flushing — core is prioritised, not monopolising. Capped by the pool, so a
/// disk with less free space than this simply hands core the whole remainder.
const CORE_PEER_RESERVE_MIN: u64 = 32 << 20;
/// Never hold back more than an eighth of the free budget for the other lane;
/// on a very tight disk core keeps the larger share.
const CORE_PEER_RESERVE_DIVISOR: u64 = 8;
/// Bytes the spool is allowed to add, always leaving the reserve untouched.
///
/// The reserve is `min(total / RESERVE_RATIO, max(FLOOR, available / 4))`: the
/// disk's 1/50 share caps it, a floor keeps a minimum tail free, and on a tight
/// disk the floor yields to a quarter of what is actually available. Because
/// the expression is capped by the 1/50 share, the reserve can never exceed the
/// free budget — the equivalent `clamp(FLOOR, available / 4)` panics outright
/// once `available < FLOOR`, and a full disk must degrade to a closed lane
/// rather than abort the node.
pub fn available_bytes(total: u64, available: u64, reserved: u64) -> u64 {
    let reserve =
        (total / RESERVE_RATIO).min((available / RESERVE_AVAILABLE_DIVISOR).max(RESERVE_FLOOR));
    available.saturating_sub(reserve).saturating_sub(reserved)
}
/// Capacity of one lane over (total, available) bytes, where `reserved_for_peer`
/// is what the *other* lane already holds in the shared counter.
///
/// The core lane additionally holds back `max(available / 8, 32 MiB)`, capped by
/// the pool, so a bursting core cannot consume the space quality observations
/// need. The detail lane holds back nothing: it is the fallback lane and lives
/// in whatever core leaves, and charging both lanes for each other would let
/// the two budgets sum past the free space.
pub fn lane_capacity(
    core: bool,
    total: u64,
    available: u64,
    reserved_for_peer: u64,
    limit: u64,
) -> u64 {
    if limit > 0 {
        return limit;
    }
    if core {
        let declared = (available / CORE_PEER_RESERVE_DIVISOR).max(CORE_PEER_RESERVE_MIN);
        // Capped by the pool (the equivalent clamp would panic once the pool is
        // smaller than the minimum).
        let peer_reserve = declared
            .min(available.saturating_sub(available_bytes(total, available, 0)))
            .max(reserved_for_peer);
        available_bytes(total, available, peer_reserve)
    } else {
        available_bytes(total, available, reserved_for_peer)
    }
}
/// Per-lane accounting shared by the two spools, which live on one filesystem
/// and therefore compete for one pool of free space. Each spool records what it
/// has written here, so a lane's capacity is a real upper bound on the pool's
/// growth rather than a per-lane view of the whole disk.
struct LaneCounter {
    used: AtomicU64,
}
impl LaneCounter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            used: AtomicU64::new(0),
        })
    }
    fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }
    fn add_used(&self, bytes: u64) {
        self.used.fetch_add(bytes, Ordering::Relaxed);
    }
    fn sub_used(&self, bytes: u64) {
        self.used.fetch_sub(bytes, Ordering::Relaxed);
    }
    /// Bytes the *other* lane holds, as seen by the core lane. Core reserves
    /// space for detail and must therefore respect detail's footprint; detail
    /// reads nothing because its budget is the fallback that absorbs whatever
    /// core leaves (and a fresh filesystem reading already reflects core).
    fn reserved_for_peer(&self, core: bool) -> u64 {
        if core { self.used() } else { 0 }
    }
}
/// One lane's durable budget: its configured ceiling, whether it is the core
/// lane (which decides who is charged), and the counter shared with the other
/// lane that records the pool's growth.
struct LaneBudget {
    limit: u64,
    core: bool,
    counter: Arc<LaneCounter>,
}
fn mace_error(e: OpCode) -> anyhow::Error {
    anyhow::anyhow!(
        "telemetry Mace 0.2 spool: {e:?}; Mace 0.1.x databases require an explicit offline rebuild; startup never deletes queued data"
    )
}
/// Distinguishes "the disk cannot take this batch" from every other spool
/// failure, so operators and alerts can point straight at capacity instead of
/// reading a generic `spool exhausted`.
#[derive(Debug)]
pub enum SpoolError {
    Capacity {
        required: u64,
        available: u64,
        core: bool,
    },
    /// A finished batch the wire cannot carry, envelope included. Its own
    /// variant because the two refusals need opposite responses: a capacity
    /// refusal is worth retrying once the spool drains, a size refusal is
    /// permanent for those bytes and the producer must split or journal them.
    BatchTooLarge { bytes: usize, limit: usize },
}
impl std::fmt::Display for SpoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity {
                required,
                available,
                core,
            } => write!(
                f,
                "telemetry spool exhausted reason=spool_capacity lane={} requiredBytes={required} availableBytes={available}",
                if *core { "core" } else { "detail" }
            ),
            Self::BatchTooLarge { bytes, limit } => write!(
                f,
                "telemetry spool refused a batch over the message budget reason=batch_too_large encodedBytes={bytes} limitBytes={limit}"
            ),
        }
    }
}
impl std::error::Error for SpoolError {}
/// Walks an error chain and returns the spool refusal it carries, if any.
fn spool_refusal<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<&'a SpoolError> {
    let mut current = Some(error);
    while let Some(err) = current {
        if let Some(spool) = err.downcast_ref::<SpoolError>() {
            return Some(spool);
        }
        current = err.source();
    }
    None
}
/// True when a spool error was a capacity rejection. Callers keep their own
/// drop/retain policy; this only classifies the cause.
pub fn is_capacity_exhausted(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(spool_refusal(error), Some(SpoolError::Capacity { .. }))
}
/// True when a spool error was the wire-size refusal rather than a full disk.
pub fn is_batch_too_large(error: &(dyn std::error::Error + 'static)) -> bool {
    matches!(spool_refusal(error), Some(SpoolError::BatchTooLarge { .. }))
}
/// One durable record of a lost batch or record.
///
/// The record is written in the same Mace transaction that removes the payload
/// it describes, so a crash can never separate the loss from its audit. It is
/// deleted only when the API's durable receipt for the gap batch carrying it
/// arrives — a best-effort marker is not proof of delivery.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LossRecord {
    /// Identity of the dropped batch (empty for a record too large to batch).
    batch_id: String,
    /// Sequence of the dropped batch (0 when it never had one).
    sequence: u64,
    /// The lane that lost the data: `core` or `detail`.
    lane: String,
    /// The telemetry kind that was lost.
    kind: String,
    /// When the lost data was produced.
    event_time: i64,
    /// The gap reason reported upstream, e.g. `unattributed_pre_assignment`.
    reason: String,
    /// The API's own message for the rejection.
    error: String,
    /// Free-form attribution, e.g. the encoded size of an oversized record.
    metadata: String,
    /// Sequence of the gap batch that reports this loss upstream.
    ///
    /// Zero means there is nothing to deliver: the loss was the refusal of a
    /// meta batch (`gap`/`alert`), which must not be answered with another
    /// marker. Such a record is evidence, not a pending delivery.
    gap_sequence: u64,
    /// True once the API permanently refused the gap marker itself.
    delivery_failed: bool,
}
fn loss_key(id: u64) -> String {
    format!("loss:{id:020}")
}
fn loss_gap_key(sequence: u64) -> String {
    format!("lossgap:{sequence:020}")
}
impl Spool {
    pub fn open(path: &Path, core: bool, limit: u64) -> anyhow::Result<Self> {
        Self::open_lane(path, core, limit, None)
    }
    /// Opens a spool, optionally attached to the other lane's usage counter so
    /// both budgets see one shared pool of free space.
    fn open_lane(
        path: &Path,
        core: bool,
        limit: u64,
        shared: Option<Arc<LaneCounter>>,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path)?;
        let mut opts = Options::new(path);
        opts.sync_on_write = true;
        opts.wal_buffer_size = 1 << 20;
        opts.wal_file_size = 4 << 20;
        opts.lru_capacity = 8 << 20;
        opts.stat_mask_cache_count = 128;
        opts.data_handle_cache_capacity = 16;
        opts.blob_handle_cache_capacity = 8;
        let db = Mace::new(opts.validate().map_err(mace_error)?).map_err(mace_error)?;
        let options = BucketOptions {
            cache_capacity: 8 << 20,
            pool_capacity: 8 << 20,
            checkpoint_size: 2 << 20,
            enable_backpressure: true,
            ..Default::default()
        };
        let bucket = match db.open_bucket("queue") {
            Ok(b) => b,
            Err(OpCode::NotFound) => db.new_bucket("queue", options).map_err(mace_error)?,
            Err(e) => return Err(mace_error(e)),
        };
        let meta = match bucket.view().map_err(mace_error)?.get(b"meta") {
            Ok(raw) => serde_json::from_slice(raw.slice())?,
            Err(OpCode::NotFound) => Meta::default(),
            Err(e) => return Err(mace_error(e)),
        };
        anyhow::ensure!(
            meta.head <= meta.tail
                && meta.acked_ahead <= meta.tail - meta.head
                && meta.acked_ahead_bytes <= meta.bytes,
            "invalid telemetry spool counters"
        );
        let counter = shared.unwrap_or_else(LaneCounter::new);
        counter.add_used(meta.bytes.saturating_add(meta.quarantined_bytes));
        // A restart must not forget that this lane lost data: the counters, the
        // last reason and the gap bounds come back from the durable meta, and
        // the process-wide gap statics are seeded from this lane's half.
        let (quarantined, retryable, terminal, oversize) = (
            meta.quarantined,
            meta.retryable_failures,
            meta.terminal_failures,
            meta.oversize_dropped,
        );
        let last_loss_reason = meta.last_loss_reason.clone();
        restore_gap_bounds(meta.gap_start, meta.gap_end, meta.gap_has_core);
        Ok(Self {
            _db: db,
            bucket,
            meta: Mutex::new(meta),
            path: path.to_owned(),
            budget: LaneBudget {
                limit,
                core,
                counter,
            },
            quarantined: AtomicU64::new(quarantined),
            retryable_failures: AtomicU64::new(retryable),
            terminal_failures: AtomicU64::new(terminal),
            oversize_dropped: AtomicU64::new(oversize),
            last_loss_reason: parking_lot::RwLock::new(last_loss_reason),
            #[cfg(test)]
            fixed_disk: None,
        })
    }
    fn capacity(&self) -> u64 {
        if self.budget.limit > 0 {
            return self.budget.limit;
        }
        let (total, available) = match self.disk_space() {
            Some(space) => space,
            None => return 0,
        };
        lane_capacity(
            self.budget.core,
            total,
            available,
            self.budget.counter.reserved_for_peer(self.budget.core),
            0,
        )
    }
    /// (total, available) of the filesystem holding this spool, using the
    /// longest matching mount point.
    fn disk_space(&self) -> Option<(u64, u64)> {
        #[cfg(test)]
        if let Some(space) = self.fixed_disk {
            return Some(space);
        }
        let disks = sysinfo::Disks::new_with_refreshed_list();
        disks
            .iter()
            .filter(|d| self.path.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len())
            .map(|d| (d.total_space(), d.available_space()))
    }
    /// Test hook: fix the (total, available) the spool believes the filesystem
    /// reports, so capacity decisions are exercised without filling a disk. A
    /// real filesystem reading already reflects this node's own writes, so the
    /// fake one is a snapshot too.
    #[cfg(test)]
    fn with_disk(mut self, total: u64, available: u64) -> Self {
        self.fixed_disk = Some((total, available));
        self
    }
    #[cfg(test)]
    fn with_peer(mut self, shared: Arc<LaneCounter>) -> Self {
        self.budget.counter = shared;
        self
    }
    pub fn append(
        &self,
        kind: &str,
        payload: Vec<u8>,
        window: i64,
        complete: bool,
        occurred: i64,
    ) -> anyhow::Result<String> {
        let binding = binding();
        let batch = pb::TelemetryBatch {
            kind: kind.into(),
            payload,
            occurred_at: occurred,
            assignment_version: binding.assignment_version,
            route_version: binding.route_version,
            window_start: window,
            window_complete: complete,
            ..Default::default()
        };
        self.append_prepared(batch)
    }
    pub fn append_prepared(&self, batch: pb::TelemetryBatch) -> anyhow::Result<String> {
        self.append_prepared_with_state(batch, None)
    }

    /// Transfer producer ownership and its cursor in the same durable commit.
    /// A refused append leaves both the queue and the producer state untouched.
    fn append_prepared_with_state(
        &self,
        mut batch: pb::TelemetryBatch,
        state: Option<(&str, &[u8])>,
    ) -> anyhow::Result<String> {
        let mut meta = self.meta.lock();
        batch.schema_version = 1;
        batch.batch_id = format!("{}:{}", meta.boot, meta.tail);
        batch.boot_id = meta.boot.clone();
        batch.sequence = meta.tail;
        if batch.source_boot_id.is_empty() {
            batch.source_boot_id = PROCESS_BOOT.clone();
        }
        // The wire carries the serialized batch, envelope included, so the cap
        // is enforced here on the finished message and for every producer kind.
        // A producer that can split does so before this call; one that cannot
        // gets a typed refusal instead of a batch the RPC layer would reject.
        let wire_bytes = batch.encoded_len();
        if wire_bytes > MAX_BATCH_ENCODED_BYTES {
            return Err(SpoolError::BatchTooLarge {
                bytes: wire_bytes,
                limit: MAX_BATCH_ENCODED_BYTES,
            }
            .into());
        }
        let encoded = zstd::encode_all(batch.encode_to_vec().as_slice(), 3)?;
        let available = self.capacity();
        let required = encoded.len() as u64;
        // A lane is out of room when the batch cannot fit in the free budget at
        // all, or when this chained queue's accumulation would pass the limit
        // it was opened with. `available` is already a per-lane budget that
        // accounts for the other lane, so neither lane can spend the other's
        // share.
        let over_limit = self.budget.limit > 0
            && meta
                .bytes
                .saturating_add(meta.quarantined_bytes)
                .saturating_add(required)
                > self.budget.limit;
        if required > available || over_limit {
            // The caller decides whether the batch is dropped (record a gap)
            // or retained in memory for the next window — a full spool is not
            // itself proof of loss.
            return Err(SpoolError::Capacity {
                required,
                available,
                core: self.budget.core,
            }
            .into());
        }
        let mut next = meta.clone();
        next.tail += 1;
        next.bytes = next.bytes.saturating_add(encoded.len() as u64);
        if meta.head == meta.tail {
            next.oldest_at = batch.occurred_at;
            next.meter_started = crate::utils::time::now_timestamp();
            next.meter_bytes = 0;
        }
        next.meter_bytes += encoded.len() as u64;
        let txn = self.bucket.begin().map_err(mace_error)?;
        txn.upsert(format!("batch:{:020}", meta.tail).as_bytes(), &encoded)
            .map_err(mace_error)?;
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        if let Some((key, raw)) = state {
            txn.upsert(format!("state:{key}").as_bytes(), raw)
                .map_err(mace_error)?;
        }
        txn.commit().map_err(mace_error)?;
        let level = capacity_level(
            next.bytes.saturating_add(next.quarantined_bytes),
            available,
            self.budget.limit,
        );
        *meta = next;
        self.budget.counter.add_used(encoded.len() as u64);
        escalate_capacity(level, self.budget.core);
        Ok(batch.batch_id)
    }
    pub fn front(&self) -> anyhow::Result<Option<pb::TelemetryBatch>> {
        self.at(0)
    }

    /// Read the offset-th unconfirmed batch. Durable receipts ahead of a
    /// rejected head are skipped, including after reopening the spool.
    pub fn at(&self, offset: u64) -> anyhow::Result<Option<pb::TelemetryBatch>> {
        let meta = self.meta.lock();
        if offset >= meta.tail - meta.head - meta.acked_ahead {
            return Ok(None);
        }
        let view = self.bucket.view().map_err(mace_error)?;
        let mut remaining = offset;
        for sequence in meta.head..meta.tail {
            match view.get(format!("ack:{sequence:020}").as_bytes()) {
                Ok(_) => continue,
                Err(OpCode::NotFound) => {}
                Err(err) => return Err(mace_error(err)),
            }
            let raw = view
                .get(format!("batch:{sequence:020}").as_bytes())
                .map_err(mace_error)?;
            if remaining == 0 {
                let decoded = zstd::decode_all(raw.slice())?;
                let batch = pb::TelemetryBatch::decode(decoded.as_slice())?;
                anyhow::ensure!(
                    batch.sequence == sequence
                        && batch.batch_id == format!("{}:{sequence}", meta.boot),
                    "telemetry spool batch identity mismatch"
                );
                return Ok(Some(batch));
            }
            remaining -= 1;
        }
        anyhow::bail!("telemetry spool pending count mismatch")
    }
    fn replay_window(&self, cursor: u64) -> anyhow::Result<(Vec<pb::TelemetryBatch>, u64)> {
        let meta = self.meta.lock();
        let start = if cursor < meta.head || cursor >= meta.tail {
            meta.head
        } else {
            cursor
        };
        let view = self.bucket.view().map_err(mace_error)?;
        let mut batches = Vec::with_capacity(REPLAY_CONCURRENCY);
        let mut sequence = start;
        while sequence < meta.tail && batches.len() < REPLAY_CONCURRENCY {
            match view.get(format!("ack:{sequence:020}").as_bytes()) {
                Ok(_) => {}
                Err(OpCode::NotFound) => {
                    let raw = view
                        .get(format!("batch:{sequence:020}").as_bytes())
                        .map_err(mace_error)?;
                    let batch =
                        pb::TelemetryBatch::decode(zstd::decode_all(raw.slice())?.as_slice())?;
                    anyhow::ensure!(
                        batch.sequence == sequence
                            && batch.batch_id == format!("{}:{sequence}", meta.boot),
                        "telemetry spool batch identity mismatch"
                    );
                    batches.push(batch);
                }
                Err(err) => return Err(mace_error(err)),
            }
            sequence += 1;
        }
        Ok((batches, sequence))
    }

    pub fn acknowledge(&self, receipt: &pb::TelemetryReceipt) -> anyhow::Result<()> {
        let mut meta = self.meta.lock();
        let sequence = receipt.sequence;
        if receipt.batch_id != format!("{}:{sequence}", meta.boot)
            || receipt.durable_at <= 0
            || receipt.backend != "direct"
            || sequence >= meta.tail
        {
            anyhow::bail!("invalid durable telemetry receipt")
        }
        if sequence < meta.head {
            return Ok(());
        }
        let txn = self.bucket.begin().map_err(mace_error)?;
        let ack_key = format!("ack:{sequence:020}");
        match txn.get(ack_key.as_bytes()) {
            Ok(_) => return Ok(()),
            Err(OpCode::NotFound) => {}
            Err(err) => return Err(mace_error(err)),
        }
        let key = format!("batch:{sequence:020}");
        let size = txn.get(key.as_bytes()).map_err(mace_error)?.slice().len() as u64;
        let mut next = meta.clone();
        if sequence == meta.head {
            next.head += 1;
            next.bytes -= size;
            txn.del(key.as_bytes()).map_err(mace_error)?;
            // A receipt for a loss marker is the only proof that the API durably
            // recorded the gap; until it arrives the audit stays on disk.
            confirm_loss_delivery(&txn, sequence)?;
            if next.head == next.tail {
                next.oldest_at = 0;
            } else {
                let raw = txn
                    .get(format!("batch:{:020}", next.head).as_bytes())
                    .map_err(mace_error)?;
                let decoded = zstd::decode_all(raw.slice())?;
                next.oldest_at = pb::TelemetryBatch::decode(decoded.as_slice())?.occurred_at;
            }
        } else {
            // A real backend receipt permits independent confirmation. Keep
            // the payload until the head catches up; never discard failures.
            txn.upsert(ack_key.as_bytes(), receipt.durable_at.to_le_bytes())
                .map_err(mace_error)?;
            next.acked_ahead += 1;
            next.acked_ahead_bytes += size;
            confirm_loss_delivery(&txn, sequence)?;
        }
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)?;
        let freed = meta.bytes - next.bytes;
        *meta = next;
        self.budget.counter.sub_used(freed);
        ALERT_LEVEL.fetch_min(
            capacity_level(meta.bytes, self.capacity(), self.budget.limit),
            Ordering::Relaxed,
        );
        Ok(())
    }

    /// Retire a bounded run of batches already confirmed out of order. A
    /// rejected head stops this operation and remains available for retry.
    fn collapse_confirmed(&self, max: usize) -> anyhow::Result<usize> {
        let mut meta = self.meta.lock();
        if meta.acked_ahead == 0 {
            return Ok(0);
        }
        let txn = self.bucket.begin().map_err(mace_error)?;
        let mut next = meta.clone();
        let mut removed = 0;
        while removed < max && next.head < next.tail {
            let ack_key = format!("ack:{:020}", next.head);
            match txn.get(ack_key.as_bytes()) {
                Ok(_) => {}
                Err(OpCode::NotFound) => break,
                Err(err) => return Err(mace_error(err)),
            }
            let key = format!("batch:{:020}", next.head);
            let size = txn.get(key.as_bytes()).map_err(mace_error)?.slice().len() as u64;
            txn.del(key.as_bytes()).map_err(mace_error)?;
            txn.del(ack_key.as_bytes()).map_err(mace_error)?;
            next.head += 1;
            next.bytes -= size;
            next.acked_ahead -= 1;
            next.acked_ahead_bytes -= size;
            removed += 1;
        }
        if removed == 0 {
            return Ok(0);
        }
        next.oldest_at = if next.head == next.tail {
            0
        } else {
            let raw = txn
                .get(format!("batch:{:020}", next.head).as_bytes())
                .map_err(mace_error)?;
            pb::TelemetryBatch::decode(zstd::decode_all(raw.slice())?.as_slice())?.occurred_at
        };
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)?;
        self.budget.counter.sub_used(meta.bytes - next.bytes);
        *meta = next;
        Ok(removed)
    }
    /// Move a permanently rejected head to durable quarantine. The original
    /// compressed envelope, rejection audit, gap marker and head advance share
    /// one transaction. A gap receipt only retires its pending report; the
    /// quarantined envelope and audit remain available for offline recovery.
    /// Archived bytes stay charged to the lane, so a full archive backpressures
    /// producers and leaves the original head untouched if the marker cannot fit.
    /// Returns false if the sequence is no longer the head.
    fn quarantine_head(&self, sequence: u64, reason: &str, error: &str) -> anyhow::Result<bool> {
        let mut meta = self.meta.lock();
        if sequence != meta.head || meta.head >= meta.tail {
            return Ok(false);
        }
        let txn = self.bucket.begin().map_err(mace_error)?;
        let key = format!("batch:{sequence:020}");
        let raw = txn.get(key.as_bytes()).map_err(mace_error)?;
        let size = raw.slice().len() as u64;
        let batch = pb::TelemetryBatch::decode(zstd::decode_all(raw.slice())?.as_slice())?;
        let kind = batch.kind.clone();
        let event_time = if batch.occurred_at > 0 {
            batch.occurred_at
        } else {
            crate::utils::time::now_timestamp()
        };
        // A marker the API refuses must not be answered with another marker:
        // otherwise one rejected audit would generate audits forever.
        let is_meta = kind == "gap" || kind == "alert";
        // Keep the original compressed envelope in the same durable transaction.
        // A gap receipt confirms the report, not delivery of this payload.
        txn.upsert(format!("quarantine:{sequence:020}").as_bytes(), raw.slice())
            .map_err(mace_error)?;
        txn.del(key.as_bytes()).map_err(mace_error)?;
        let mut next = meta.clone();
        next.head += 1;
        next.bytes -= size;
        next.quarantined_bytes = next.quarantined_bytes.saturating_add(size);
        next.quarantined = self.quarantined.load(Ordering::Relaxed) + 1;
        next.retryable_failures = self.retryable_failures.load(Ordering::Relaxed);
        next.terminal_failures = self.terminal_failures.load(Ordering::Relaxed);
        next.oversize_dropped = self.oversize_dropped.load(Ordering::Relaxed);
        next.last_loss_reason = reason.to_owned();
        next.gap_start = if meta.gap_start == 0 {
            event_time
        } else {
            meta.gap_start
        };
        next.gap_end = meta.gap_end.max(event_time);
        next.gap_has_core = meta.gap_has_core || kind == "core" || kind == "bandwidth";
        let loss = LossRecord {
            batch_id: batch.batch_id.clone(),
            sequence,
            lane: self.lane().to_owned(),
            kind: kind.clone(),
            event_time,
            reason: reason.to_owned(),
            error: error.to_owned(),
            metadata: format!("retainedBytes={size}"),
            ..Default::default()
        };
        if is_meta {
            // The audit this marker carried is now undeliverable: keep the
            // record as evidence and stop re-reporting it.
            match txn.get(loss_gap_key(sequence).as_bytes()) {
                Ok(carried) => {
                    let carried_key = carried.slice().to_vec();
                    if let Ok(record) = txn.get(&carried_key) {
                        let mut failed: LossRecord = serde_json::from_slice(record.slice())?;
                        failed.delivery_failed = true;
                        txn.upsert(&carried_key, serde_json::to_vec(&failed)?)
                            .map_err(mace_error)?;
                    }
                    txn.del(loss_gap_key(sequence).as_bytes())
                        .map_err(mace_error)?;
                }
                Err(OpCode::NotFound) => {}
                Err(err) => return Err(mace_error(err)),
            }
        }
        let audit = serde_json::to_vec(&loss)?;
        txn.upsert(format!("quarantineaudit:{sequence:020}").as_bytes(), &audit)
            .map_err(mace_error)?;
        next.quarantined_bytes = next.quarantined_bytes.saturating_add(audit.len() as u64);
        let (gap_sequence, gap_bytes) = self.write_loss(&txn, &mut next, loss, !is_meta)?;
        let required = next.bytes.saturating_add(next.quarantined_bytes);
        let extra_bytes = gap_bytes.saturating_add(audit.len() as u64);
        let available = if self.budget.limit > 0 {
            self.budget
                .limit
                .saturating_sub(meta.bytes.saturating_add(meta.quarantined_bytes))
        } else {
            self.capacity()
        };
        if extra_bytes > available || (self.budget.limit > 0 && required > self.budget.limit) {
            return Err(SpoolError::Capacity {
                required: extra_bytes,
                available,
                core: self.budget.core,
            }
            .into());
        }
        next.oldest_at = if next.head == next.tail {
            0
        } else if gap_sequence != 0 && next.head == gap_sequence {
            // The lane was empty and the only batch left is the marker written
            // in this very transaction, which this snapshot cannot read back.
            event_time
        } else {
            let raw = txn
                .get(format!("batch:{:020}", next.head).as_bytes())
                .map_err(mace_error)?;
            pb::TelemetryBatch::decode(zstd::decode_all(raw.slice())?.as_slice())?.occurred_at
        };
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)?;
        // The payload remains charged in quarantine; only the archive audit
        // and the marker add bytes after this durable ownership transfer.
        self.budget.counter.add_used(extra_bytes);
        self.quarantined.fetch_add(1, Ordering::Relaxed);
        *self.last_loss_reason.write() = reason.to_owned();
        *meta = next;
        drop(meta);
        // The process-wide view and the operator log. The durable report is the
        // loss record plus the gap batch committed above, so this deliberately
        // does not emit a second, best-effort marker for the same incident.
        note_gap(&kind, event_time, reason);
        Ok(true)
    }

    /// Append the durable loss record and, unless this is a meta batch, the gap
    /// batch that reports it upstream. Both join the caller's transaction, so
    /// the loss and its audit share one commit.
    ///
    /// This helper writes within the caller's transaction. Quarantine checks
    /// the additional archive and marker bytes before committing; the
    /// oversized-record journal retains its existing audit policy.
    ///
    /// Returns the gap batch's sequence and its spool bytes (both 0 when no
    /// marker was created).
    fn write_loss(
        &self,
        txn: &TxnKV<'_>,
        next: &mut Meta,
        mut loss: LossRecord,
        allow_gap_batch: bool,
    ) -> anyhow::Result<(u64, u64)> {
        let id = next.journal_id;
        next.journal_id += 1;
        let mut gap_sequence = 0;
        let mut gap_bytes = 0;
        if allow_gap_batch {
            let sequence = next.tail;
            let batch = gap_batch_for_loss(next, &loss, sequence);
            let encoded = zstd::encode_all(batch.encode_to_vec().as_slice(), 3)?;
            txn.upsert(format!("batch:{sequence:020}").as_bytes(), &encoded)
                .map_err(mace_error)?;
            txn.upsert(loss_gap_key(sequence).as_bytes(), loss_key(id).as_bytes())
                .map_err(mace_error)?;
            next.tail += 1;
            next.bytes = next.bytes.saturating_add(encoded.len() as u64);
            next.meter_bytes += encoded.len() as u64;
            loss.gap_sequence = sequence;
            gap_sequence = sequence;
            gap_bytes = encoded.len() as u64;
        }
        txn.upsert(loss_key(id).as_bytes(), serde_json::to_vec(&loss)?)
            .map_err(mace_error)?;
        Ok((gap_sequence, gap_bytes))
    }

    /// Journal a loss that is not a queued batch — today, one record too large
    /// to fit in any batch. The record and the gap marker that reports it commit
    /// together, so the loss cannot be counted without being attributable.
    fn journal_loss(&self, loss: LossRecord) -> anyhow::Result<()> {
        let mut meta = self.meta.lock();
        let txn = self.bucket.begin().map_err(mace_error)?;
        let mut next = meta.clone();
        next.quarantined = self.quarantined.load(Ordering::Relaxed);
        next.retryable_failures = self.retryable_failures.load(Ordering::Relaxed);
        next.terminal_failures = self.terminal_failures.load(Ordering::Relaxed);
        next.oversize_dropped = self.oversize_dropped.load(Ordering::Relaxed);
        next.last_loss_reason = loss.reason.clone();
        next.gap_start = if meta.gap_start == 0 {
            loss.event_time
        } else {
            meta.gap_start
        };
        next.gap_end = meta.gap_end.max(loss.event_time);
        next.gap_has_core = meta.gap_has_core || loss.kind == "core" || loss.kind == "bandwidth";
        let (_, gap_bytes) = self.write_loss(&txn, &mut next, loss.clone(), true)?;
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)?;
        self.budget.counter.add_used(gap_bytes);
        *self.last_loss_reason.write() = next.last_loss_reason.clone();
        *meta = next;
        drop(meta);
        note_gap(&loss.kind, loss.event_time, &loss.reason);
        Ok(())
    }

    /// Journal one record too large for any batch: count it, keep every field an
    /// auditor needs, and report it upstream through its own gap marker instead
    /// of shipping an empty batch or a bare error.
    fn journal_oversize(&self, kind: &str, encoded_bytes: usize) -> anyhow::Result<()> {
        self.oversize_dropped.fetch_add(1, Ordering::Relaxed);
        self.journal_loss(LossRecord {
            lane: self.lane().to_owned(),
            kind: kind.to_owned(),
            event_time: crate::utils::time::now_timestamp(),
            reason: format!("{OVERSIZE_REASON} encodedBytes={encoded_bytes}"),
            metadata: format!("encodedBytes={encoded_bytes}"),
            ..Default::default()
        })
    }

    fn lane(&self) -> &'static str {
        if self.budget.core { "core" } else { "detail" }
    }

    /// The durable loss record with this journal id, if it has not been
    /// delivered yet.
    #[cfg(test)]
    fn loss_record(&self, id: u64) -> anyhow::Result<Option<LossRecord>> {
        let view = self.bucket.view().map_err(mace_error)?;
        match view.get(loss_key(id).as_bytes()) {
            Ok(raw) => Ok(Some(serde_json::from_slice(raw.slice())?)),
            Err(OpCode::NotFound) => Ok(None),
            Err(err) => Err(mace_error(err)),
        }
    }

    #[cfg(test)]
    fn quarantined_batch(&self, sequence: u64) -> anyhow::Result<pb::TelemetryBatch> {
        let view = self.bucket.view().map_err(mace_error)?;
        let raw = view
            .get(format!("quarantine:{sequence:020}").as_bytes())
            .map_err(mace_error)?;
        Ok(pb::TelemetryBatch::decode(
            zstd::decode_all(raw.slice())?.as_slice(),
        )?)
    }

    /// This lane's durable gap bounds, `(start, end, has_core)`.
    #[cfg(test)]
    fn gap_bounds(&self) -> (i64, i64, bool) {
        let meta = self.meta.lock();
        (meta.gap_start, meta.gap_end, meta.gap_has_core)
    }

    /// P1-2 — count one failed upload per class. Retryable failures keep their
    /// payload; terminal ones end up quarantined when they are the head.
    ///
    /// The counts live in memory between journal writes and become durable with
    /// the next quarantine or oversize journal, which persists the live values.
    fn note_failure(&self, class: UploadClass) {
        match class {
            UploadClass::Retryable => self.retryable_failures.fetch_add(1, Ordering::Relaxed),
            UploadClass::Terminal(_) => self.terminal_failures.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn save_state(&self, key: &str, raw: &[u8]) -> anyhow::Result<()> {
        let txn = self.bucket.begin().map_err(mace_error)?;
        txn.upsert(format!("state:{key}").as_bytes(), raw)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)
    }
    pub fn load_state(&self, key: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let view = self.bucket.view().map_err(mace_error)?;
        match view.get(format!("state:{key}").as_bytes()) {
            Ok(v) => Ok(Some(v.to_vec())),
            Err(OpCode::NotFound) => Ok(None),
            Err(e) => Err(mace_error(e)),
        }
    }
    pub fn boot_id(&self) -> String {
        self.meta.lock().boot.clone()
    }
    pub fn snapshot(&self) -> serde_json::Value {
        let meta = self.meta.lock();
        let available = self.capacity();
        let pending = meta.tail - meta.head - meta.acked_ahead;
        let elapsed = crate::utils::time::now_timestamp() - meta.meter_started;
        let rate = if pending > 0 && elapsed > 0 {
            meta.meter_bytes / elapsed as u64
        } else {
            0
        };
        serde_json::json!({
            "pendingBatches": pending,
            "pendingBytes": meta.bytes - meta.acked_ahead_bytes,
            "retainedBytes": meta.bytes,
            "quarantineBytes": meta.quarantined_bytes,
            "remainingBytes": available,
            "capacityLevel": capacity_level(meta.bytes.saturating_add(meta.quarantined_bytes), available, self.budget.limit),
            "oldestAt": if pending > 0 { meta.oldest_at } else { 0 },
            "exhaustEtaSecs": if rate > 0 { available / rate.max(1) } else { 0 },
            "lastFailureAt": LAST_FAILURE.load(Ordering::Relaxed),
            // P1-2: the queue's own position and its loss/refusal counters.
            "head": meta.head,
            "tail": meta.tail,
            "ackedAhead": meta.acked_ahead,
            "quarantinedBatches": self.quarantined.load(Ordering::Relaxed),
            "retryableFailures": self.retryable_failures.load(Ordering::Relaxed),
            "terminalFailures": self.terminal_failures.load(Ordering::Relaxed),
            "oversizeDropped": self.oversize_dropped.load(Ordering::Relaxed),
            // Total losses ever journaled by this lane, delivered or not.
            "journaledLosses": meta.journal_id,
            "lastQuarantineReason": self.last_loss_reason.read().clone(),
        })
    }
}
pub fn capacity_level(used: u64, remaining: u64, limit: u64) -> u8 {
    let total = if limit > 0 {
        limit
    } else {
        used.saturating_add(remaining)
    };
    if total == 0 {
        return 97;
    }
    let percent = used.saturating_mul(100) / total;
    if percent >= 97 {
        97
    } else if percent >= 90 {
        90
    } else if percent >= 80 {
        80
    } else {
        0
    }
}
struct Queues {
    core: Arc<Spool>,
    detail: Arc<Spool>,
}
static QUEUES: OnceLock<Queues> = OnceLock::new();
static RT: OnceLock<tokio::runtime::Handle> = OnceLock::new();
static LAST_FAILURE: AtomicI64 = AtomicI64::new(0);
static GAP_START: AtomicI64 = AtomicI64::new(0);
static GAP_END: AtomicI64 = AtomicI64::new(0);
static GAP_CORE: AtomicI64 = AtomicI64::new(0);
static GAP_REPORTED: AtomicI64 = AtomicI64::new(0);
static ALERT_LEVEL: AtomicU8 = AtomicU8::new(0);
/// P1-2: the reason of the most recent gap, so a status reader can see *why*
/// data was dropped without correlating logs. The durable trail is the loss
/// record and the gap marker that carries it upstream.
static LAST_GAP_REASON: std::sync::LazyLock<parking_lot::RwLock<String>> =
    std::sync::LazyLock::new(|| parking_lot::RwLock::new(String::new()));

pub fn last_gap_reason() -> String {
    LAST_GAP_REASON.read().clone()
}

/// Update the process-wide gap view and log the incident.
///
/// This is the in-process half only: it emits nothing. A caller that owns a
/// durable loss record must not emit a second, best-effort marker for the same
/// incident, or one loss would be reported twice upstream.
fn note_gap(kind: &str, at: i64, reason: &str) {
    *LAST_GAP_REASON.write() = reason.to_owned();
    let _ = GAP_START.compare_exchange(0, at, Ordering::Relaxed, Ordering::Relaxed);
    GAP_END.fetch_max(at, Ordering::Relaxed);
    if kind == "core" || kind == "bandwidth" {
        GAP_CORE.store(1, Ordering::Relaxed);
    }
    error!(
        kind,
        reason,
        at,
        "TELEMETRY DATA GAP: service continues; affected billing windows must remain unsettled"
    );
}

/// Record a gap for a loss that has no durable journal of its own, updating the
/// process-wide view and emitting one best-effort marker per incident start
/// (re-emits only after the marker itself was acknowledged upstream).
pub fn record_gap(kind: &str, at: i64, reason: &str) {
    note_gap(kind, at, reason);
    let start = GAP_START.load(Ordering::Relaxed);
    if GAP_REPORTED.fetch_max(start, Ordering::Relaxed) < start {
        emit_meta("gap", kind, reason, start, at);
    }
}

/// Seed the process-wide gap statics from a lane's durable bounds. Merging, not
/// assigning, keeps the union across both lanes and across repeated opens.
fn restore_gap_bounds(start: i64, end: i64, has_core: bool) {
    if start > 0 {
        let _ = GAP_START.compare_exchange(0, start, Ordering::Relaxed, Ordering::Relaxed);
        GAP_END.fetch_max(end, Ordering::Relaxed);
    }
    if has_core {
        GAP_CORE.store(1, Ordering::Relaxed);
    }
}

/// The gap batch that carries one loss upstream. Its envelope is the same shape
/// `emit_meta` uses for a gap marker, so the API persists it as a `telemetry_gap`
/// row; a real timestamp is allowed for a gap range where a completion marker
/// would have to sit on the five-minute grid.
fn gap_batch_for_loss(meta: &Meta, loss: &LossRecord, sequence: u64) -> pb::TelemetryBatch {
    pb::TelemetryBatch {
        schema_version: 1,
        batch_id: format!("{}:{sequence}", meta.boot),
        boot_id: meta.boot.clone(),
        sequence,
        source_boot_id: PROCESS_BOOT.clone(),
        kind: "gap".into(),
        has_gap: true,
        gap_kind: loss.kind.clone(),
        gap_reason: loss.reason.clone(),
        window_start: loss.event_time,
        occurred_at: loss.event_time,
        ..Default::default()
    }
}

/// Mark a loss delivered. A real receipt for its gap batch means the API has
/// durably recorded the gap, so the audit record can go. Both keys live in this
/// lane's bucket, so this joins the acknowledgement transaction.
fn confirm_loss_delivery(txn: &TxnKV<'_>, gap_sequence: u64) -> anyhow::Result<()> {
    let gap_key = loss_gap_key(gap_sequence);
    match txn.get(gap_key.as_bytes()) {
        Ok(carried) => {
            let carried_key = carried.slice().to_vec();
            txn.del(carried_key.as_slice()).map_err(mace_error)?;
            txn.del(gap_key.as_bytes()).map_err(mace_error)?;
        }
        Err(OpCode::NotFound) => {}
        Err(err) => return Err(mace_error(err)),
    }
    Ok(())
}
fn escalate_capacity(level: u8, core: bool) {
    if level < 80 {
        return;
    }
    if ALERT_LEVEL.fetch_max(level, Ordering::Relaxed) < level {
        emit_meta(
            "alert",
            "capacity",
            &format!(
                "persistent {} buffer at {}% of budget",
                if core { "core" } else { "detail" },
                level
            ),
            0,
            crate::utils::time::now_timestamp(),
        );
    }
}
fn emit_meta(kind: &str, gap_kind: &str, reason: &str, start: i64, end: i64) {
    let batch = pb::TelemetryBatch {
        kind: kind.into(),
        has_gap: kind == "gap",
        gap_kind: gap_kind.into(),
        gap_reason: reason.into(),
        window_start: start,
        occurred_at: end,
        ..Default::default()
    };
    let spawn = |handle: &tokio::runtime::Handle| {
        handle.spawn(async move {
            // Best-effort: if the spool itself is exhausted the marker stays
            // local and the incident surfaces via metrics and missing windows.
            let _ = enqueue_prepared(batch).await;
        })
    };
    if let Some(handle) = RT.get() {
        spawn(handle);
    } else if let Ok(handle) = tokio::runtime::Handle::try_current() {
        spawn(&handle);
    } else {
        debug!(kind, "telemetry meta marker dropped: no async runtime");
    }
}
pub fn initialize() -> anyhow::Result<()> {
    let path =
        crate::storage_migration::store_root(&NodePaths::current().data_dir()).join("telemetry");
    let core = Arc::new(Spool::open(&path.join("core"), true, 0)?);
    // Both lanes write to one filesystem, so the detail lane joins the core
    // lane's accounting: core's footprint is subtracted from detail's budget
    // and vice versa, while each keeps its own ceiling.
    let detail = Arc::new(Spool::open_lane(
        &path.join("detail"),
        false,
        0,
        Some(core.budget.counter.clone()),
    )?);
    QUEUES
        .set(Queues { core, detail })
        .map_err(|_| anyhow::anyhow!("telemetry already initialized"))
}
fn queue(kind: &str) -> anyhow::Result<Arc<Spool>> {
    let q = QUEUES
        .get()
        .ok_or_else(|| anyhow::anyhow!("telemetry spool unavailable"))?;
    Ok(
        if kind == "bandwidth"
            || kind == "node_metric"
            || kind == "gap"
            || kind == "alert"
            || kind == "daily"
            || kind == "unique_ip"
        {
            q.core.clone()
        } else {
            q.detail.clone()
        },
    )
}
pub fn core_boot_id() -> String {
    QUEUES.get().map(|q| q.core.boot_id()).unwrap_or_default()
}
pub async fn enqueue<M: Message>(
    kind: &str,
    message: &M,
    window: i64,
    complete: bool,
) -> anyhow::Result<()> {
    let payload = message.encode_to_vec();
    let spool = queue(kind)?;
    let kind = kind.to_owned();
    let at = crate::utils::time::now_timestamp();
    tokio::task::spawn_blocking(move || spool.append(&kind, payload, window, complete, at))
        .await??;
    Ok(())
}

/// Probe observations that a failed enqueue did not durably accept.
#[derive(Debug)]
pub struct ProbeEnqueueError {
    source: anyhow::Error,
    unsent: Vec<pb::ProbeObservation>,
    lost: usize,
}

impl ProbeEnqueueError {
    fn new(source: anyhow::Error, unsent: Vec<pb::ProbeObservation>, lost: usize) -> Self {
        Self {
            source,
            unsent,
            lost,
        }
    }

    pub fn unsent(&self) -> &[pb::ProbeObservation] {
        &self.unsent
    }

    /// Observations whose append task panicked or was cancelled after the
    /// records left the caller's ownership; they cannot be retried and must be
    /// counted as a gap rather than silently forgotten.
    pub fn lost(&self) -> usize {
        self.lost
    }

    pub fn into_unsent(self) -> Vec<pb::ProbeObservation> {
        self.unsent
    }
}

impl std::fmt::Display for ProbeEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.source)
    }
}

impl std::error::Error for ProbeEnqueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Enqueue probe observations in wire-sized chunks while retaining exact
/// ownership: a failed append returns only the records it did not persist,
/// and records already committed or journaled as oversize are never retried.
pub async fn enqueue_probe_observations(
    observations: Vec<pb::ProbeObservation>,
    window: i64,
) -> Result<(), ProbeEnqueueError> {
    let spool = match queue("probe_observation") {
        Ok(spool) => spool,
        Err(source) => return Err(ProbeEnqueueError::new(source, observations, 0)),
    };
    enqueue_probe_observations_on(spool, observations, window).await
}

async fn enqueue_probe_observations_on(
    spool: Arc<Spool>,
    observations: Vec<pb::ProbeObservation>,
    window: i64,
) -> Result<(), ProbeEnqueueError> {
    let (batches, oversize) = split_records(observations, payload_budget("probe_observation"));
    let mut oversize = oversize.into_iter();
    while let Some(observation) = oversize.next() {
        let cost = record_cost(&observation);
        if let Err(source) =
            journal_oversize_on(Arc::clone(&spool), "probe_observation", cost).await
        {
            let mut unsent = vec![observation];
            unsent.extend(oversize);
            for batch in batches {
                unsent.extend(batch);
            }
            return Err(ProbeEnqueueError::new(source, unsent, 0));
        }
    }
    let mut batches = batches.into_iter();
    while let Some(batch) = batches.next() {
        if let Err(mut error) = append_probe_chunk(Arc::clone(&spool), batch, window).await {
            for remaining in batches {
                error.unsent.extend(remaining);
            }
            return Err(error);
        }
    }
    Ok(())
}

async fn append_probe_chunk(
    spool: Arc<Spool>,
    observations: Vec<pb::ProbeObservation>,
    window: i64,
) -> Result<(), ProbeEnqueueError> {
    let count = observations.len();
    let at = crate::utils::time::now_timestamp();
    let batch = pb::ProbeObservationBatch { observations };
    match tokio::task::spawn_blocking(move || {
        let payload = batch.encode_to_vec();
        match spool.append("probe_observation", payload, window, false, at) {
            Ok(_) => Ok(()),
            Err(error) => Err((error, batch.observations)),
        }
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err((source, observations))) => Err(ProbeEnqueueError::new(source, observations, 0)),
        Err(join) => Err(ProbeEnqueueError::new(
            anyhow::anyhow!("probe append task failed: {join}"),
            Vec::new(),
            count,
        )),
    }
}

/// Metric rows that a failed enqueue did not durably accept.
#[derive(Debug)]
pub struct MetricEnqueueError {
    source: anyhow::Error,
    unsent: Box<pb::UploadMetricStatsRequest>,
    lost: usize,
}

impl MetricEnqueueError {
    fn new(source: anyhow::Error, unsent: pb::UploadMetricStatsRequest, lost: usize) -> Self {
        Self {
            source,
            unsent: Box::new(unsent),
            lost,
        }
    }

    /// Rows whose append task panicked or was cancelled after ownership moved
    /// out of the reporter; they cannot be retried and must be counted as a gap.
    pub fn lost(&self) -> usize {
        self.lost
    }

    pub fn into_unsent(self) -> pb::UploadMetricStatsRequest {
        *self.unsent
    }
}

impl std::fmt::Display for MetricEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.source)
    }
}

impl std::error::Error for MetricEnqueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Enqueue one server's metric rows in wire-sized requests. Each chunk keeps
/// the request header so rows remain attributable to the same item/version and
/// period. A failed append returns only the rows the spool did not persist.
pub async fn enqueue_metric_stat(
    request: pb::UploadMetricStatsRequest,
) -> Result<(), MetricEnqueueError> {
    let spool = match queue("metric_stat") {
        Ok(spool) => spool,
        Err(source) => return Err(MetricEnqueueError::new(source, request, 0)),
    };
    enqueue_metric_stat_on(spool, request).await
}

async fn enqueue_metric_stat_on(
    spool: Arc<Spool>,
    request: pb::UploadMetricStatsRequest,
) -> Result<(), MetricEnqueueError> {
    let header = MetricRequestHeader {
        server_id: request.server_id,
        time: request.time,
        count: request.count,
        total: request.total,
        version: request.version,
        item_id: request.item_id,
        keep_keys: request.keep_keys,
        revision: request.revision,
        window_start: request.window_start,
        window_end: request.window_end,
    };
    // The inner request header travels inside the telemetry payload, so it
    // must come out of the row budget; otherwise a full batch plus header
    // could exceed the wire cap and be refused forever.
    let budget =
        payload_budget("metric_stat").saturating_sub(header.build(Vec::new()).encoded_len());
    let (batches, oversize) = split_records(request.metric_stats, budget.max(1));
    let mut oversize = oversize.into_iter();
    while let Some(stat) = oversize.next() {
        let cost = record_cost(&stat);
        if let Err(source) = journal_oversize_on(Arc::clone(&spool), "metric_stat", cost).await {
            let mut unsent = vec![stat];
            unsent.extend(oversize);
            for batch in batches {
                unsent.extend(batch);
            }
            return Err(MetricEnqueueError::new(source, header.build(unsent), 0));
        }
    }
    let mut batches = batches.into_iter();
    while let Some(batch) = batches.next() {
        let request = header.build(batch);
        if let Err(mut error) = append_metric_chunk(Arc::clone(&spool), request).await {
            for remaining in batches {
                error.unsent.metric_stats.extend(remaining);
            }
            return Err(error);
        }
    }
    Ok(())
}

struct MetricRequestHeader {
    server_id: i64,
    time: String,
    count: i64,
    total: f32,
    version: i32,
    item_id: i64,
    keep_keys: Vec<String>,
    revision: u64,
    window_start: i64,
    window_end: i64,
}

impl MetricRequestHeader {
    fn build(&self, metric_stats: Vec<pb::UploadingMetricStat>) -> pb::UploadMetricStatsRequest {
        pb::UploadMetricStatsRequest {
            server_id: self.server_id,
            time: self.time.clone(),
            count: self.count,
            total: self.total,
            version: self.version,
            item_id: self.item_id,
            metric_stats,
            keep_keys: self.keep_keys.clone(),
            revision: self.revision,
            window_start: self.window_start,
            window_end: self.window_end,
        }
    }
}

async fn append_metric_chunk(
    spool: Arc<Spool>,
    request: pb::UploadMetricStatsRequest,
) -> Result<(), MetricEnqueueError> {
    let count = request.metric_stats.len();
    let at = crate::utils::time::now_timestamp();
    match tokio::task::spawn_blocking(move || {
        let payload = request.encode_to_vec();
        match spool.append("metric_stat", payload, 0, false, at) {
            Ok(_) => Ok(()),
            Err(error) => Err(MetricEnqueueError::new(error, request, 0)),
        }
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(join) => Err(MetricEnqueueError::new(
            anyhow::anyhow!("metric append task failed: {join}"),
            pb::UploadMetricStatsRequest::default(),
            count,
        )),
    }
}

/// Daily rows that a failed enqueue did not durably accept.
#[derive(Debug)]
pub struct DailyEnqueueError {
    source: anyhow::Error,
    unsent: pb::UploadServerDailyStatsRequest,
    lost: usize,
}

impl DailyEnqueueError {
    fn new(source: anyhow::Error, unsent: pb::UploadServerDailyStatsRequest, lost: usize) -> Self {
        Self {
            source,
            unsent,
            lost,
        }
    }

    pub fn lost(&self) -> usize {
        self.lost
    }

    pub fn into_unsent(self) -> pb::UploadServerDailyStatsRequest {
        self.unsent
    }
}

impl std::fmt::Display for DailyEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.source)
    }
}

impl std::error::Error for DailyEnqueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.source()
    }
}

/// Enqueue daily server and domain rows in wire-sized requests. Server rows
/// and domain rows are chunked independently; a failed append returns exactly
/// the rows the spool did not persist, so the reporter never replays committed
/// chunks or drops the batches behind them.
pub async fn enqueue_daily_stats(
    request: pb::UploadServerDailyStatsRequest,
) -> Result<(), DailyEnqueueError> {
    let spool = match queue("daily") {
        Ok(spool) => spool,
        Err(source) => return Err(DailyEnqueueError::new(source, request, 0)),
    };
    enqueue_daily_stats_on(spool, request).await
}

async fn enqueue_daily_stats_on(
    spool: Arc<Spool>,
    request: pb::UploadServerDailyStatsRequest,
) -> Result<(), DailyEnqueueError> {
    let budget = payload_budget("daily");
    let (stats_batches, stats_oversize) =
        split_server_records(request.stats, budget, |row| row.server_id);
    let (domain_batches, domain_oversize) =
        split_server_records(request.domain_stats, budget, |row| row.server_id);

    let mut stats_oversize = stats_oversize.into_iter();
    while let Some(stat) = stats_oversize.next() {
        let cost = record_cost(&stat);
        if let Err(source) = journal_oversize_on(Arc::clone(&spool), "daily", cost).await {
            let mut stats = vec![stat];
            stats.extend(stats_oversize);
            for batch in stats_batches {
                stats.extend(batch);
            }
            let mut domain_stats = domain_oversize;
            for batch in domain_batches {
                domain_stats.extend(batch);
            }
            return Err(DailyEnqueueError::new(
                source,
                daily_request(stats, domain_stats),
                0,
            ));
        }
    }

    let mut domain_oversize = domain_oversize.into_iter();
    while let Some(stat) = domain_oversize.next() {
        let cost = record_cost(&stat);
        if let Err(source) = journal_oversize_on(Arc::clone(&spool), "daily", cost).await {
            let stats = stats_batches.into_iter().flatten().collect::<Vec<_>>();
            let mut domain_stats = vec![stat];
            domain_stats.extend(domain_oversize);
            for batch in domain_batches {
                domain_stats.extend(batch);
            }
            return Err(DailyEnqueueError::new(
                source,
                daily_request(stats, domain_stats),
                0,
            ));
        }
    }

    let mut chunks = stats_batches
        .into_iter()
        .map(DailyChunk::Stats)
        .chain(domain_batches.into_iter().map(DailyChunk::Domains));
    while let Some(chunk) = chunks.next() {
        let request = chunk.into_request();
        if let Err(mut error) = append_daily_chunk(Arc::clone(&spool), request).await {
            for remaining in chunks {
                match remaining {
                    DailyChunk::Stats(rows) => error.unsent.stats.extend(rows),
                    DailyChunk::Domains(rows) => error.unsent.domain_stats.extend(rows),
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

enum DailyChunk {
    Stats(Vec<pb::ServerDailyStat>),
    Domains(Vec<pb::upload_server_daily_stats_request::DomainStat>),
}

impl DailyChunk {
    fn into_request(self) -> pb::UploadServerDailyStatsRequest {
        match self {
            DailyChunk::Stats(stats) => daily_request(stats, Vec::new()),
            DailyChunk::Domains(domain_stats) => daily_request(Vec::new(), domain_stats),
        }
    }
}

fn daily_request(
    stats: Vec<pb::ServerDailyStat>,
    domain_stats: Vec<pb::upload_server_daily_stats_request::DomainStat>,
) -> pb::UploadServerDailyStatsRequest {
    pb::UploadServerDailyStatsRequest {
        stats,
        domain_stats,
    }
}

async fn append_daily_chunk(
    spool: Arc<Spool>,
    request: pb::UploadServerDailyStatsRequest,
) -> Result<(), DailyEnqueueError> {
    let count = request.stats.len() + request.domain_stats.len();
    let at = crate::utils::time::now_timestamp();
    match tokio::task::spawn_blocking(move || {
        let payload = request.encode_to_vec();
        match spool.append("daily", payload, 0, false, at) {
            Ok(_) => Ok(()),
            Err(error) => Err((error, request)),
        }
    })
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err((source, request))) => Err(DailyEnqueueError::new(source, request, 0)),
        Err(join) => Err(DailyEnqueueError::new(
            anyhow::anyhow!("daily append task failed: {join}"),
            pb::UploadServerDailyStatsRequest::default(),
            count,
        )),
    }
}

pub async fn enqueue_prepared(batch: pb::TelemetryBatch) -> anyhow::Result<()> {
    let spool = queue(&batch.kind)?;
    tokio::task::spawn_blocking(move || spool.append_prepared(batch)).await??;
    Ok(())
}
pub async fn enqueue_prepared_with_state(
    batch: pb::TelemetryBatch,
    key: &str,
    raw: Vec<u8>,
) -> anyhow::Result<()> {
    let spool = queue(&batch.kind)?;
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || {
        spool.append_prepared_with_state(batch, Some((&key, &raw)))
    })
    .await??;
    Ok(())
}

pub async fn save_state(key: &str, raw: Vec<u8>) -> anyhow::Result<()> {
    let spool = queue("bandwidth")?;
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || spool.save_state(&key, &raw)).await??;
    Ok(())
}
pub fn load_state(key: &str) -> anyhow::Result<Option<Vec<u8>>> {
    queue("bandwidth")?.load_state(key)
}
pub fn snapshot() -> serde_json::Value {
    match QUEUES.get() {
        Some(q) => snapshot_of(&q.core, &q.detail),
        None => serde_json::json!({"status": "unavailable"}),
    }
}

/// The telemetry block of the node status: both lanes' own snapshots plus the
/// process-wide gap state. Split out of [`snapshot`] so the shape can be
/// asserted without a live spool pair.
fn snapshot_of(core: &Spool, detail: &Spool) -> serde_json::Value {
    serde_json::json!({
        "core": core.snapshot(),
        "detail": detail.snapshot(),
        "gapStart": GAP_START.load(Ordering::Relaxed),
        "gapEnd": GAP_END.load(Ordering::Relaxed),
        "gapHasCore": GAP_CORE.load(Ordering::Relaxed) != 0,
        // P1-2: why the last gap happened, and how many single records were too
        // large to ever be batched. The counters are per lane and summed here
        // for the node view.
        "lastGapReason": last_gap_reason(),
        "oversizeDroppedObservations": core.oversize_dropped.load(Ordering::Relaxed)
            + detail.oversize_dropped.load(Ordering::Relaxed),
    })
}
type Upload = (
    String,
    u64,
    tokio::task::JoinHandle<anyhow::Result<pb::TelemetryReceipt>>,
);

/// P0-2 — how an upload result must be treated.
///
/// The classification is the difference between "retry and keep the payload"
/// and "this batch will never be accepted, drop it with a durable audit".
/// Retrying a terminal rejection forever is what turns one poisoned batch into a
/// frozen lane: the head never advances, every later batch is re-sent in vain,
/// and the spool grows until the disk fills. Destroying data the API never
/// refused is the opposite failure and just as bad, so the rules are:
///
/// * The stable `zelvik-error` code wins over the gRPC category, because the
///   category is only a bucket while the code is the API's statement about this
///   batch.
/// * Without a stable code, only an unambiguous payload refusal drops data.
/// * Anything an authorization decision can reverse keeps the queue: credentials
///   and instance authorizations are re-issued, and the batches already on disk
///   are still the node's to deliver once they are.
///
/// | signal | class | why |
/// | --- | --- | --- |
/// | `zelvik-error: telemetry.assignment_before_first` | Terminal | data older than the node's first assignment; nobody can accept it |
/// | `zelvik-error: control.batch_too_large` | Terminal | resending these bytes cannot shrink them |
/// | `zelvik-error: control.rate_limited` | Retryable | the API was busy, not unwilling |
/// | `zelvik-error: control.credential_*` / `control.instance_*` | Retryable | a revocable authorization must not cost the queue |
/// | `zelvik-retryable: false` (other codes) | Terminal | the server stated the class for a code we do not special-case |
/// | `InvalidArgument` | Terminal | the payload violates the schema; resending cannot fix it |
/// | `AlreadyExists` | Terminal | the batch identity is already taken by other content |
/// | `OutOfRange` | Terminal | a field is outside the accepted range |
/// | `Unimplemented` | Terminal | the API does not serve this kind |
/// | `ResourceExhausted` + a size message | Terminal | a size refusal is permanent for these bytes |
/// | `ResourceExhausted` otherwise | Retryable | a plain resource shortage is the server's problem |
/// | `FailedPrecondition` without the stable code | Retryable | the same category also means "not ready yet" |
/// | `PermissionDenied`, `Unauthenticated` | Retryable | assignment and credential checks are revocable |
/// | `Unavailable`, `DeadlineExceeded`, `Internal`, `Unknown`, `Aborted`, `Cancelled` | Retryable | transport or server fault, unrelated to the payload |
/// | *(client 30 s timeout — no status at all)* | Retryable | no answer is not a rejection |
/// | *(malformed receipt, spool error, task panic)* | Retryable | nothing proves the API refused the payload |
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UploadClass {
    Retryable,
    Terminal(TerminalReason),
}

/// The stable code the API uses for a batch older than this node's first
/// telemetry assignment. The code travels in the `zelvik-error` trailer, which
/// is what makes the rejection attributable; the gRPC category alone is not.
const ERROR_ASSIGNMENT_BEFORE_FIRST: &str = "telemetry.assignment_before_first";

/// Why a batch was rejected for good. Each reason maps to the gap reason the
/// loss record carries, so a dropped batch is attributable from the status alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalReason {
    /// The batch predates this node's first telemetry assignment.
    PreAssignment,
    DeploymentMissing,
    AssignmentInvalid,
    RouteInvalid,
    InvalidArgument,
    AlreadyExists,
    OutOfRange,
    Unimplemented,
    ResourceExhausted,
    /// The server said `zelvik-retryable: false` without a code we know.
    ServerRejected,
}

impl TerminalReason {
    /// The gap reason recorded for a quarantined batch.
    fn gap_reason(self) -> &'static str {
        match self {
            Self::PreAssignment => "unattributed_pre_assignment",
            Self::DeploymentMissing => "unattributed_deployment",
            Self::AssignmentInvalid => "invalid_assignment",
            Self::RouteInvalid => "invalid_route",
            Self::InvalidArgument => "terminal_invalid_argument",
            Self::AlreadyExists => "terminal_already_exists",
            Self::OutOfRange => "terminal_out_of_range",
            Self::Unimplemented => "terminal_unimplemented",
            Self::ResourceExhausted => "terminal_resource_exhausted",
            Self::ServerRejected => "terminal_server_rejected",
        }
    }
}

/// Read one trailer from a status, if the API sent it.
fn trailer<'a>(status: &'a tonic::Status, key: &str) -> Option<&'a str> {
    status
        .metadata()
        .get(key)
        .and_then(|value| value.to_str().ok())
}

/// Codes whose refusal is an authorization decision that a later credential or
/// assignment push can reverse.
fn is_authorization_code(code: &str) -> bool {
    [
        ControlCode::CredentialMissing,
        ControlCode::CredentialInvalid,
        ControlCode::CredentialExpired,
        ControlCode::InstanceUnknown,
        ControlCode::InstanceSuspended,
        ControlCode::InstanceRevoked,
    ]
    .iter()
    .any(|candidate| candidate.as_str() == code)
}

/// True when a `ResourceExhausted` message names a size, which makes the refusal
/// about these bytes rather than about the server's spare capacity.
fn is_size_refusal(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["size", "too large", "exceeds", "byte limit"]
        .iter()
        .any(|marker| message.contains(marker))
}

/// The classification table above, as code.
fn classify_status(status: &tonic::Status) -> UploadClass {
    let code = trailer(status, TRAILER_ERROR);
    if code == Some(ERROR_ASSIGNMENT_BEFORE_FIRST) {
        return UploadClass::Terminal(TerminalReason::PreAssignment);
    }
    if code.is_some_and(is_authorization_code) {
        // Revoked and expired credentials are re-issued, and an instance
        // authorization can be restored. The batches already on disk are still
        // the node's to deliver, so a revocable refusal never drops them —
        // even when the server marks it non-retryable.
        return UploadClass::Retryable;
    }
    if code == Some(ControlCode::BatchTooLarge.as_str()) {
        // The protocol marks `control.batch_too_large` retryable, meaning
        // "retry with a smaller batch"; this batch's bytes can never shrink.
        return UploadClass::Terminal(TerminalReason::ResourceExhausted);
    }
    if code == Some(ControlCode::RateLimited.as_str()) || code == Some("control.clock_skew") {
        return UploadClass::Retryable;
    }
    if trailer(status, TRAILER_RETRYABLE) == Some("false") {
        let reason = match code {
            Some("telemetry.deployment_missing") => Some(TerminalReason::DeploymentMissing),
            Some("telemetry.assignment_invalid") => Some(TerminalReason::AssignmentInvalid),
            Some("telemetry.route_invalid") => Some(TerminalReason::RouteInvalid),
            _ => None,
        };
        if let Some(reason) = reason {
            return UploadClass::Terminal(reason);
        }
    }
    if let Some(retryable) = trailer(status, TRAILER_RETRYABLE) {
        // The server stated the class for a code we do not special-case.
        return if retryable == "false" {
            UploadClass::Terminal(TerminalReason::ServerRejected)
        } else {
            UploadClass::Retryable
        };
    }
    match status.code() {
        tonic::Code::Unavailable
        | tonic::Code::DeadlineExceeded
        | tonic::Code::Internal
        | tonic::Code::Unknown
        | tonic::Code::Aborted
        | tonic::Code::Cancelled => UploadClass::Retryable,
        // The API's own payload checks, which carry no stable code: the batch
        // violates the schema, so resending it repeats the refusal.
        tonic::Code::InvalidArgument => UploadClass::Terminal(TerminalReason::InvalidArgument),
        tonic::Code::AlreadyExists => UploadClass::Terminal(TerminalReason::AlreadyExists),
        tonic::Code::OutOfRange => UploadClass::Terminal(TerminalReason::OutOfRange),
        tonic::Code::Unimplemented => UploadClass::Terminal(TerminalReason::Unimplemented),
        // A size refusal is permanent for these bytes; a plain resource shortage
        // is the server's, not the batch's.
        tonic::Code::ResourceExhausted => {
            if is_size_refusal(status.message()) {
                UploadClass::Terminal(TerminalReason::ResourceExhausted)
            } else {
                UploadClass::Retryable
            }
        }
        // Assignment and route checks arrive as `PermissionDenied`, but the
        // credential behind them is revocable and may be re-issued, so the queue
        // is kept rather than destroyed. `FailedPrecondition` without the stable
        // code is "not ready yet" for the same reason.
        _ => UploadClass::Retryable,
    }
}

/// Classify a failed upload. Only a status the API chose may drop data: the
/// 30 s client timeout, a cancelled task, a malformed receipt and a spool error
/// all arrive as plain `anyhow` errors and stay retryable.
fn classify_upload_failure(error: &anyhow::Error) -> UploadClass {
    match error.downcast_ref::<tonic::Status>() {
        Some(status) => classify_status(status),
        None => UploadClass::Retryable,
    }
}

/// What the replay loop must do after a window was settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowAction {
    /// The window made progress — confirmations, or a quarantined head that
    /// unblocked the lane: take the next window immediately.
    Continue,
    /// Part of the window landed: the rejected batches stay for the next pass.
    Pause,
    /// Nothing landed: exponential backoff.
    Backoff,
}

struct WindowOutcome {
    confirmed: usize,
    /// First failure in sequence order, with its class, for logging and backoff.
    failure: Option<(UploadClass, anyhow::Error)>,
    /// Sequence of the head batch quarantined in this window, if any.
    quarantined: Option<u64>,
}

impl WindowOutcome {
    fn action(&self) -> WindowAction {
        if self.quarantined.is_some() || self.failure.is_none() {
            WindowAction::Continue
        } else if self.confirmed > 0 {
            WindowAction::Pause
        } else {
            WindowAction::Backoff
        }
    }
}

/// Upload a prefetched window, acknowledge every receipt, and quarantine the
/// head when the API rejected it permanently.
///
/// A terminal failure only ever removes the *head*: the head is the batch that
/// blocks the lane, and a batch that failed behind it may still be accepted
/// once it is the head. Anything that is not the head keeps its payload and its
/// place, so this can never discard data the API did not explicitly refuse.
async fn upload_window(spool: Arc<Spool>, uploads: Vec<Upload>) -> WindowOutcome {
    let mut confirmed = 0;
    let mut failure: Option<(UploadClass, anyhow::Error)> = None;
    let mut rejected_head: Option<(u64, &'static str, String)> = None;
    for (batch_id, sequence, handle) in uploads {
        let result = async {
            let receipt = handle.await??;
            anyhow::ensure!(
                receipt.batch_id == batch_id && receipt.sequence == sequence,
                "telemetry response does not match the uploaded batch"
            );
            let ack = spool.clone();
            tokio::task::spawn_blocking(move || ack.acknowledge(&receipt)).await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => confirmed += 1,
            Err(err) => {
                let class = classify_upload_failure(&err);
                spool.note_failure(class);
                warn!(%batch_id, sequence, class=?class, error=%err, "telemetry upload remains unconfirmed");
                if rejected_head.is_none()
                    && let UploadClass::Terminal(reason) = class
                {
                    // Uploads are in sequence order, so the first terminal
                    // failure is the lowest one: the only quarantine candidate.
                    // The API's own message is kept with the durable audit.
                    rejected_head = Some((sequence, reason.gap_reason(), err.to_string()));
                }
                if failure.is_none() {
                    failure = Some((class, err));
                }
            }
        }
    }
    let mut quarantined = None;
    if let Some((sequence, reason, error)) = rejected_head {
        let head = spool.clone();
        let removed =
            tokio::task::spawn_blocking(move || head.quarantine_head(sequence, reason, &error))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
        match removed {
            Ok(true) => quarantined = Some(sequence),
            // Not the head any more: it was confirmed meanwhile, so there is
            // nothing to drop.
            Ok(false) => {}
            Err(err) => {
                warn!(sequence, error=%err, "telemetry quarantine failed; original head retained");
                if failure.is_none() {
                    failure = Some((UploadClass::Retryable, err));
                }
            }
        }
    }
    WindowOutcome {
        confirmed,
        failure,
        quarantined,
    }
}

pub async fn replay(api: ApiConfig, core: bool) {
    let _ = RT.set(tokio::runtime::Handle::current());
    let Some(queues) = QUEUES.get() else {
        error!("telemetry replay cannot start: persistent spool unavailable");
        return;
    };
    let spool = if core {
        queues.core.clone()
    } else {
        queues.detail.clone()
    };
    let mut failures = 0u32;
    let mut cursor = 0;
    loop {
        let compact = spool.clone();
        if let Err(err) = tokio::task::spawn_blocking(move || compact.collapse_confirmed(256))
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        {
            backoff(core, &mut failures, err).await;
            continue;
        }
        // Pre-fetch unconfirmed batches. A failed older batch stays on disk,
        // while later durable receipts are remembered independently.
        let read = spool.clone();
        let (prefetched, next_cursor) =
            match tokio::task::spawn_blocking(move || read.replay_window(cursor))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                Ok(window) => window,
                Err(err) => {
                    backoff(core, &mut failures, err).await;
                    continue;
                }
            };
        if prefetched.is_empty() {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        let client = match SharedRpcClient::get(&api).await {
            Ok(client) => client.as_rpc_client(),
            Err(err) => {
                backoff(core, &mut failures, err).await;
                continue;
            }
        };
        // 并发上传：每个批次一次 RPC，但不再互相等待 RTT。
        let mut uploads = Vec::with_capacity(prefetched.len());
        for batch in prefetched {
            let client = client.clone();
            let batch_id = batch.batch_id.clone();
            let sequence = batch.sequence;
            let handle = tokio::spawn(async move {
                let mut service = client.telemetry_service();
                let receipt =
                    tokio::time::timeout(Duration::from_secs(30), service.upload_batch(batch))
                        .await;
                match receipt {
                    Ok(Ok(receipt)) => Ok(receipt.into_inner()),
                    Ok(Err(err)) => Err(anyhow::Error::from(err)),
                    Err(err) => Err(anyhow::Error::from(err)),
                }
            });
            uploads.push((batch_id, sequence, handle));
        }
        let outcome = upload_window(spool.clone(), uploads).await;
        cursor = next_cursor;
        if let Some(sequence) = outcome.quarantined {
            // P0-3: a permanently rejected head was dropped with an auditable
            // gap, so the batches behind it keep flowing in this same pass
            // instead of waiting out a backoff they did not cause.
            warn!(
                core,
                sequence,
                confirmed = outcome.confirmed,
                "telemetry head batch quarantined; gap recorded and the lane continues"
            );
        }
        match outcome.action() {
            // Progress: a poisoned head is gone, or everything landed.
            WindowAction::Continue => failures = 0,
            WindowAction::Pause => {
                // A poison head must not impose minute-long retries on healthy
                // observations. Errors remain visible and their payloads retained.
                let confirmed = outcome.confirmed;
                let error = outcome
                    .failure
                    .as_ref()
                    .map(|(_, err)| err.to_string())
                    .unwrap_or_default();
                LAST_FAILURE.store(crate::utils::time::now_timestamp(), Ordering::Relaxed);
                warn!(
                    core,
                    confirmed,
                    error,
                    "telemetry replay partially confirmed; rejected batches retained"
                );
                failures = failures.saturating_add(1);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            WindowAction::Backoff => {
                let err = outcome
                    .failure
                    .map(|(_, err)| err)
                    .unwrap_or_else(|| anyhow::anyhow!("telemetry upload window failed"));
                backoff(core, &mut failures, err).await;
            }
        }
    }
}

/// backoff 记录并退避一次上传失败；首次失败升级为 ERROR，避免静默积压。
async fn backoff(core: bool, failures: &mut u32, err: anyhow::Error) {
    if *failures == 0 {
        LAST_FAILURE.store(crate::utils::time::now_timestamp(), Ordering::Relaxed);
        error!(core, error=%err, "URGENT: telemetry write failed; retained on node until durable receipt");
    } else {
        warn!(core, error=%err, "telemetry batch remains pending");
    }
    let n = failures.saturating_add(1);
    *failures = n;
    tokio::time::sleep(Duration::from_secs((1u64 << n.min(6)).min(60))).await;
}
/// P0-4 — encoded-size ceiling for one telemetry batch.
///
/// The wire carries the *serialized* `TelemetryBatch`, envelope included, so the
/// cap applies to the whole message and every producer must keep its payload
/// inside [`payload_budget`]. `RPC_NODE_STREAM_MESSAGE_BYTES` is 8 MiB, and the
/// API's own payload field allows 48 MiB, but the smallest of those limits is
/// the one that decides; a quarter of the message budget stays free for the gRPC
/// frame prefix, the credential interceptor and any envelope the API adds around
/// the batch. Compressing a too-large batch would hide the problem rather than
/// fix it, so the cap is measured on the uncompressed serialization.
const MAX_BATCH_ENCODED_BYTES: usize = RPC_NODE_STREAM_MESSAGE_BYTES / 4 * 3;
/// The payload field's own cost inside a `TelemetryBatch`: the field tag plus a
/// length varint, which is at most five bytes for a payload under 2^32.
const PAYLOAD_FIELD_OVERHEAD: usize = 1 + 5;
/// Gap reason for a single record too large to fit in any batch.
const OVERSIZE_REASON: &str = "observation_over_batch_cap";

/// What one record costs inside a length-delimited repeated field: the field
/// tag, the payload's length prefix and the payload — exactly its contribution
/// to the request's serialized size, and it allocates nothing (`encoded_len`
/// walks the message, it does not encode it).
fn record_cost<M: Message>(record: &M) -> usize {
    prost::encoding::message::encoded_len(1, record)
}

/// Budget one producer's payload may use so that the batch carrying it stays
/// under [`MAX_BATCH_ENCODED_BYTES`] once serialized *with its envelope*.
///
/// The envelope is measured, not guessed: `encoded_len` walks the message
/// without encoding it, and the boot id, kind and binding versions are the ones
/// this process would actually write. The two fields the envelope does not have
/// yet — the sequence and the batch id — are measured at their worst case, and
/// the empty payload field is not counted by `encoded_len` at all, which is why
/// [`PAYLOAD_FIELD_OVERHEAD`] is added back.
fn payload_budget(kind: &str) -> usize {
    let binding = binding();
    let envelope = pb::TelemetryBatch {
        schema_version: 1,
        batch_id: format!("{}:{:020}", "0".repeat(36), u64::MAX),
        boot_id: "0".repeat(36),
        sequence: u64::MAX,
        kind: kind.into(),
        occurred_at: i64::MIN,
        window_start: i64::MIN,
        source_boot_id: "0".repeat(36),
        assignment_version: binding.assignment_version,
        route_version: binding.route_version,
        ..Default::default()
    };
    MAX_BATCH_ENCODED_BYTES
        .saturating_sub(envelope.encoded_len())
        .saturating_sub(PAYLOAD_FIELD_OVERHEAD)
}

/// Split a producer's pending records into batches whose payload stays inside
/// `budget` — the per-kind budget from [`payload_budget`], which already leaves
/// room for the envelope.
///
/// The count caps (512 access logs, 128 node logs) remain an upper bound on a
/// batch; this cap is what keeps a batch of *large* records — a fat `paramsJSON`,
/// a long log line — below the RPC message limit. Records that do not fit in a
/// batch by themselves are returned separately: the caller journals each one
/// through the loss path, so a single giant record can neither fail nor block
/// the records that arrived with it.
fn split_server_records<M: Message>(
    records: Vec<M>,
    budget: usize,
    server_id: impl Fn(&M) -> i64,
) -> (Vec<Vec<M>>, Vec<M>) {
    let mut servers = std::collections::BTreeMap::<i64, Vec<M>>::new();
    for record in records {
        servers.entry(server_id(&record)).or_default().push(record);
    }
    let mut batches = Vec::new();
    let mut oversized = Vec::new();
    for records in servers.into_values() {
        let (chunks, rejected) = split_records(records, budget);
        batches.extend(chunks);
        oversized.extend(rejected);
    }
    (batches, oversized)
}

fn split_records<M: Message>(records: Vec<M>, budget: usize) -> (Vec<Vec<M>>, Vec<M>) {
    let mut batches = Vec::new();
    let mut current: Vec<M> = Vec::new();
    let mut used = 0usize;
    let mut oversize = Vec::new();
    for record in records {
        let cost = record_cost(&record);
        if cost > budget {
            oversize.push(record);
            continue;
        }
        if used + cost > budget && !current.is_empty() {
            batches.push(std::mem::take(&mut current));
            used = 0;
        }
        used += cost;
        current.push(record);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    (batches, oversize)
}

/// Journal the loss of one record that cannot fit in any batch.
///
/// The journal entry and the gap marker that reports it are durable, so an
/// oversized record is neither an empty batch nor a bare error: it is a loss
/// with a reason and a size, counted on the lane that produced it.
async fn journal_oversize(kind: &str, encoded_bytes: usize) -> anyhow::Result<()> {
    let spool = queue(kind)?;
    journal_oversize_on(spool, kind, encoded_bytes).await
}

async fn journal_oversize_on(
    spool: Arc<Spool>,
    kind: &str,
    encoded_bytes: usize,
) -> anyhow::Result<()> {
    let kind = kind.to_owned();
    tokio::task::spawn_blocking(move || spool.journal_oversize(&kind, encoded_bytes)).await??;
    Ok(())
}

pub async fn access_logs(mut rx: tokio::sync::mpsc::Receiver<pb::HttpAccessLog>) {
    let mut buffer = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            log = rx.recv() => {
                match log {
                    Some(log) => buffer.push(log),
                    None => {
                        if !buffer.is_empty() {
                            persist_access(&mut buffer).await;
                        }
                        return;
                    }
                }
                if buffer.len() >= 512 {
                    persist_access(&mut buffer).await;
                }
            }
            _ = tick.tick() => {
                if !buffer.is_empty() {
                    persist_access(&mut buffer).await;
                }
            }
        }
    }
}
async fn persist_access(buffer: &mut Vec<pb::HttpAccessLog>) {
    let (batches, oversize) = split_server_records(
        std::mem::take(buffer),
        payload_budget("access_log"),
        |log| log.server_id,
    );
    for log in &oversize {
        // A record too large for any batch is a loss; journal it durably rather
        // than failing the batch around it or emitting an empty one.
        if let Err(e) = journal_oversize("access_log", record_cost(log)).await {
            error!(error=%e, "telemetry oversize loss journal write failed");
        }
    }
    for http_access_logs in batches {
        let request = pb::CreateHttpAccessLogsRequest { http_access_logs };
        if let Err(e) = enqueue("access_log", &request, 0, false).await {
            record_gap(
                "access_log",
                crate::utils::time::now_timestamp(),
                &e.to_string(),
            );
        }
    }
}
pub async fn node_logs(mut rx: tokio::sync::mpsc::Receiver<pb::NodeLog>) {
    let mut buffer = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            log = rx.recv() => {
                match log {
                    Some(log) => buffer.push(log),
                    None => return,
                }
                if buffer.len() >= 128 {
                    persist_node_logs(&mut buffer).await;
                }
            }
            _ = tick.tick() => {
                if !buffer.is_empty() {
                    persist_node_logs(&mut buffer).await;
                }
            }
        }
    }
}
async fn persist_node_logs(buffer: &mut Vec<pb::NodeLog>) {
    let (batches, oversize) =
        split_server_records(std::mem::take(buffer), payload_budget("node_log"), |log| {
            log.server_id
        });
    for log in &oversize {
        // A record too large for any batch is a loss; journal it durably rather
        // than failing the batch around it or emitting an empty one.
        if let Err(e) = journal_oversize("node_log", record_cost(log)).await {
            error!(error=%e, "telemetry oversize loss journal write failed");
        }
    }
    for node_logs in batches {
        let request = pb::CreateNodeLogsRequest { node_logs };
        if let Err(e) = enqueue("node_log", &request, 0, false).await {
            record_gap(
                "node_log",
                crate::utils::time::now_timestamp(),
                &e.to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    /// The live acceptance disk: 240 GiB with 3.8 GiB free.
    const TIGHT_TOTAL: u64 = 240 * GIB;
    const TIGHT_AVAILABLE: u64 = 3_800 * MIB + 300 * MIB;

    /// A spool whose filesystem readings are fixed, for capacity cases that
    /// cannot be built by actually filling a disk.
    /// The bytes core holds back for the detail lane, as `lane_capacity`
    /// composes it.
    fn core_reserved_bytes(total: u64, available: u64) -> u64 {
        let declared = (available / CORE_PEER_RESERVE_DIVISOR).max(CORE_PEER_RESERVE_MIN);
        declared.min(available.saturating_sub(available_bytes(total, available, 0)))
    }

    /// Bytes that survive zstd level 3, so `append` measures them at face
    /// value. A zeros payload compresses thousands of times over and could
    /// never trip a capacity guard.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn spool_with_disk(core: bool, limit: u64, total: u64, available: u64) -> (Spool, PathBuf) {
        let path = std::env::temp_dir().join(format!("telemetry-cap-{}", uuid::Uuid::new_v4()));
        let spool = Spool::open(&path, core, limit)
            .unwrap()
            .with_disk(total, available);
        (spool, path)
    }

    fn receipt_for(batch: &pb::TelemetryBatch) -> pb::TelemetryReceipt {
        pb::TelemetryReceipt {
            batch_id: batch.batch_id.clone(),
            sequence: batch.sequence,
            backend: "direct".into(),
            durable_at: 1000,
        }
    }

    #[test]
    fn out_of_order_receipts_preserve_rejected_head_across_reopen() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        for at in [100, 200, 300] {
            spool
                .append("probe_observation", vec![at as u8], 0, false, at)
                .unwrap();
        }
        let first = spool.at(0).unwrap().unwrap();
        let second = spool.at(1).unwrap().unwrap();
        let third = spool.at(2).unwrap().unwrap();
        spool.acknowledge(&receipt_for(&second)).unwrap();
        spool.acknowledge(&receipt_for(&third)).unwrap();
        spool.acknowledge(&receipt_for(&second)).unwrap();
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        assert!(spool.at(1).unwrap().is_none());
        assert_eq!(spool.front().unwrap().unwrap().batch_id, first.batch_id);
        assert_eq!(spool.collapse_confirmed(256).unwrap(), 0);
        drop(spool);

        let spool = Spool::open(&path, false, MIB).unwrap();
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        assert!(spool.at(1).unwrap().is_none());
        assert_eq!(spool.front().unwrap().unwrap().payload, first.payload);
        spool.acknowledge(&receipt_for(&first)).unwrap();
        assert_eq!(spool.collapse_confirmed(1).unwrap(), 1);
        assert_eq!(spool.collapse_confirmed(256).unwrap(), 1);
        assert!(spool.front().unwrap().is_none());
        assert_eq!(spool.snapshot()["pendingBytes"], 0);
        assert_eq!(spool.snapshot()["retainedBytes"], 0);
        assert_eq!(spool.budget.counter.used(), 0);
        spool.acknowledge(&receipt_for(&third)).unwrap();
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn failed_head_does_not_discard_later_upload_receipts() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        let spool = Arc::new(spool);
        spool.append("node_log", vec![1], 0, false, 100).unwrap();
        spool
            .append("probe_observation", vec![2], 0, false, 200)
            .unwrap();
        let first = spool.at(0).unwrap().unwrap();
        let second = spool.at(1).unwrap().unwrap();
        let good = receipt_for(&second);
        let uploads = vec![
            (
                first.batch_id.clone(),
                first.sequence,
                tokio::spawn(async {
                    Err(anyhow::Error::from(tonic::Status::unavailable(
                        "assignment unavailable",
                    )))
                }),
            ),
            (
                second.batch_id,
                second.sequence,
                tokio::spawn(async { Ok(good) }),
            ),
        ];
        let outcome = upload_window(spool.clone(), uploads).await;
        assert_eq!(outcome.confirmed, 1);
        assert!(outcome.quarantined.is_none());
        assert!(
            outcome
                .failure
                .as_ref()
                .unwrap()
                .1
                .to_string()
                .contains("assignment unavailable")
        );
        assert_eq!(spool.front().unwrap().unwrap().batch_id, first.batch_id);
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Build a status the way the API sends one: a gRPC category plus the
    /// stable `zelvik-error` / `zelvik-retryable` trailers.
    fn status_with(
        code: tonic::Code,
        message: &str,
        trailers: &[(&'static str, &str)],
    ) -> tonic::Status {
        let mut metadata = tonic::metadata::MetadataMap::new();
        for (key, value) in trailers {
            metadata.insert(*key, value.parse().unwrap());
        }
        tonic::Status::with_metadata(code, message, metadata)
    }

    #[test]
    fn attribution_requires_terminal_evidence_and_clock_skew_retries() {
        for (code, reason) in [
            (
                "telemetry.deployment_missing",
                TerminalReason::DeploymentMissing,
            ),
            (
                "telemetry.assignment_invalid",
                TerminalReason::AssignmentInvalid,
            ),
            ("telemetry.route_invalid", TerminalReason::RouteInvalid),
        ] {
            for retryable in ["true", "false"] {
                let status = status_with(
                    tonic::Code::FailedPrecondition,
                    "attribution refused",
                    &[(TRAILER_ERROR, code), (TRAILER_RETRYABLE, retryable)],
                );
                assert_eq!(
                    classify_status(&status),
                    if retryable == "true" {
                        UploadClass::Retryable
                    } else {
                        UploadClass::Terminal(reason)
                    }
                );
            }
        }
        let clock = status_with(
            tonic::Code::InvalidArgument,
            "future clock",
            &[
                (TRAILER_ERROR, "control.clock_skew"),
                (TRAILER_RETRYABLE, "true"),
            ],
        );
        assert_eq!(classify_status(&clock), UploadClass::Retryable);
    }

    #[test]
    fn server_split_preserves_every_row_and_separates_sites() {
        let rows = [1, 2, 1, 3, 2]
            .map(|server_id| pb::HttpAccessLog {
                server_id,
                ..Default::default()
            })
            .to_vec();
        let (batches, oversized) = split_server_records(rows, 1024, |row| row.server_id);
        assert!(oversized.is_empty());
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 5);
        assert_eq!(batches.len(), 3);
        assert!(
            batches
                .iter()
                .all(|batch| batch.iter().all(|row| row.server_id == batch[0].server_id))
        );
    }

    /// P0-2: transport and server faults keep their payload.
    #[test]
    fn transient_statuses_are_retryable() {
        use tonic::Code;
        for code in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Internal,
            Code::Unknown,
            Code::Aborted,
            Code::Cancelled,
        ] {
            assert_eq!(
                classify_status(&status_with(code, "", &[])),
                UploadClass::Retryable,
                "{code:?} is a transient failure and must be retried"
            );
        }
        // A server fault the API calls permanent is still permanent.
        assert_eq!(
            classify_status(&status_with(
                Code::Internal,
                "poisoned batch",
                &[(TRAILER_RETRYABLE, "false")]
            )),
            UploadClass::Terminal(TerminalReason::ServerRejected)
        );
    }

    /// P0-2: the one permanent `FailedPrecondition` is the one the API marks
    /// with its stable code. The same category without it means "not ready yet",
    /// and destroying a queue over that would lose deliverable data.
    #[test]
    fn assignment_before_first_is_terminal_only_with_its_stable_code() {
        use tonic::Code;
        let marked = status_with(
            Code::FailedPrecondition,
            "batch older than the node's first telemetry assignment",
            &[
                (TRAILER_ERROR, ERROR_ASSIGNMENT_BEFORE_FIRST),
                (TRAILER_RETRYABLE, "false"),
            ],
        );
        assert_eq!(
            classify_status(&marked),
            UploadClass::Terminal(TerminalReason::PreAssignment)
        );
        assert_eq!(
            TerminalReason::PreAssignment.gap_reason(),
            "unattributed_pre_assignment"
        );
        assert_eq!(
            classify_status(&status_with(
                Code::FailedPrecondition,
                "assignment not loaded yet",
                &[]
            )),
            UploadClass::Retryable
        );
        assert_eq!(
            classify_status(&status_with(
                Code::FailedPrecondition,
                "assignment not loaded yet",
                &[(TRAILER_RETRYABLE, "true")]
            )),
            UploadClass::Retryable
        );
    }

    /// P0-2: an authorization decision is revocable, so it never costs the
    /// queue — not even when the server marks it non-retryable.
    #[test]
    fn authorization_failures_keep_the_queue() {
        use tonic::Code;
        for code in [
            ControlCode::CredentialMissing,
            ControlCode::CredentialInvalid,
            ControlCode::CredentialExpired,
            ControlCode::InstanceUnknown,
            ControlCode::InstanceSuspended,
            ControlCode::InstanceRevoked,
        ] {
            let status = status_with(
                code.grpc_code(),
                code.template(),
                &[(TRAILER_ERROR, code.as_str()), (TRAILER_RETRYABLE, "false")],
            );
            assert_eq!(
                classify_status(&status),
                UploadClass::Retryable,
                "{code:?} must not destroy the queue: the credential can rotate"
            );
        }
        // The transport credential, and the API's own assignment/route checks
        // (which carry no stable code), are revocable too.
        assert_eq!(
            classify_status(&status_with(
                Code::Unauthenticated,
                "credential missing",
                &[]
            )),
            UploadClass::Retryable
        );
        assert_eq!(
            classify_status(&status_with(
                Code::PermissionDenied,
                "invalid telemetry assignment",
                &[]
            )),
            UploadClass::Retryable
        );
    }

    /// P0-2: `ResourceExhausted` is two different failures. A size refusal is
    /// permanent for these bytes; a rate limit is not.
    #[test]
    fn size_refusals_are_terminal_and_rate_limits_are_not() {
        use tonic::Code;
        let too_large = status_with(
            Code::ResourceExhausted,
            "batch exceeds 6291456 bytes",
            &[
                (TRAILER_ERROR, ControlCode::BatchTooLarge.as_str()),
                (TRAILER_RETRYABLE, "true"),
            ],
        );
        assert_eq!(
            classify_status(&too_large),
            UploadClass::Terminal(TerminalReason::ResourceExhausted),
            "resending the same bytes cannot shrink them"
        );
        let limited = status_with(
            Code::ResourceExhausted,
            "instance rate limit exceeded",
            &[
                (TRAILER_ERROR, ControlCode::RateLimited.as_str()),
                (TRAILER_RETRYABLE, "true"),
            ],
        );
        assert_eq!(classify_status(&limited), UploadClass::Retryable);
        // Without a code, a message that names a size is still a size refusal...
        assert_eq!(
            classify_status(&status_with(
                Code::ResourceExhausted,
                "message size exceeds the limit",
                &[]
            )),
            UploadClass::Terminal(TerminalReason::ResourceExhausted)
        );
        // ...while a plain resource shortage is the server's problem.
        assert_eq!(
            classify_status(&status_with(
                Code::ResourceExhausted,
                "backend overloaded",
                &[]
            )),
            UploadClass::Retryable
        );
    }

    /// P0-2: payload refusals with no stable code are terminal, because
    /// resending the same bytes repeats them.
    #[test]
    fn payload_refusals_are_terminal() {
        use tonic::Code;
        for (code, reason) in [
            (Code::InvalidArgument, TerminalReason::InvalidArgument),
            (Code::AlreadyExists, TerminalReason::AlreadyExists),
            (Code::OutOfRange, TerminalReason::OutOfRange),
            (Code::Unimplemented, TerminalReason::Unimplemented),
        ] {
            assert_eq!(
                classify_status(&status_with(code, "refused", &[])),
                UploadClass::Terminal(reason),
                "{code:?} is a permanent rejection and must be quarantined"
            );
        }
        // Errors that are not a status at all never drop data.
        assert_eq!(
            classify_upload_failure(&anyhow::anyhow!("telemetry spool: not found")),
            UploadClass::Retryable
        );
    }

    /// P0-2: the 30 s client timeout is not a rejection, so it stays retryable.
    #[tokio::test]
    async fn client_timeout_is_retryable_not_terminal() {
        let elapsed = tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>())
            .await
            .expect_err("the deadline must fire");
        let err = anyhow::Error::from(elapsed);
        assert!(
            err.downcast_ref::<tonic::Status>().is_none(),
            "a client timeout carries no gRPC status"
        );
        assert_eq!(classify_upload_failure(&err), UploadClass::Retryable);
    }

    /// P0-3: a permanently rejected head is quarantined with a durable audit,
    /// and the healthy batches behind it still upload in the same pass.
    #[tokio::test]
    async fn terminal_head_is_quarantined_and_later_batches_still_upload() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        let spool = Arc::new(spool);
        for at in [100, 200, 300, 400] {
            spool
                .append("access_log", vec![at as u8], 0, false, at)
                .unwrap();
        }
        let batches: Vec<pb::TelemetryBatch> =
            (0..4).map(|i| spool.at(i).unwrap().unwrap()).collect();
        let mut uploads = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            let handle = if index == 0 {
                let status = status_with(
                    tonic::Code::FailedPrecondition,
                    "batch older than the node's first telemetry assignment",
                    &[
                        (TRAILER_ERROR, ERROR_ASSIGNMENT_BEFORE_FIRST),
                        (TRAILER_RETRYABLE, "false"),
                    ],
                );
                tokio::spawn(async move { Err(anyhow::Error::from(status)) })
            } else {
                let receipt = receipt_for(batch);
                tokio::spawn(async move { Ok(receipt) })
            };
            uploads.push((batch.batch_id.clone(), batch.sequence, handle));
        }
        let outcome = upload_window(spool.clone(), uploads).await;
        assert_eq!(
            outcome.confirmed, 3,
            "the three healthy batches behind the rejected head must upload"
        );
        assert_eq!(outcome.quarantined, Some(batches[0].sequence));
        assert_eq!(outcome.action(), WindowAction::Continue);
        // The quarantined batch is gone and the head moved past it; the three
        // healthy batches were acknowledged ahead of the new head (they were
        // uploaded while the rejected batch was still the head), and the gap
        // marker reporting the loss is queued behind them.
        let snapshot = spool.snapshot();
        assert_eq!(snapshot["head"], batches[0].sequence + 1);
        assert_eq!(snapshot["tail"], batches[0].sequence + 5);
        assert_eq!(snapshot["ackedAhead"], 3);
        assert_eq!(snapshot["pendingBatches"], 1);
        // The loss is auditable: counted, reasoned, and journaled in full.
        assert_eq!(snapshot["quarantinedBatches"], 1);
        assert_eq!(
            snapshot["lastQuarantineReason"],
            "unattributed_pre_assignment"
        );
        assert_eq!(snapshot["terminalFailures"], 1);
        assert_eq!(snapshot["retryableFailures"], 0);
        assert_eq!(snapshot["journaledLosses"], 1);
        let loss = spool
            .loss_record(0)
            .unwrap()
            .expect("the loss is journaled");
        assert_eq!(loss.batch_id, batches[0].batch_id);
        assert_eq!(loss.sequence, batches[0].sequence);
        assert_eq!(loss.lane, "detail");
        assert_eq!(loss.kind, "access_log");
        assert_eq!(loss.event_time, 100);
        assert_eq!(loss.reason, "unattributed_pre_assignment");
        assert!(loss.error.contains("first telemetry assignment"));
        assert!(loss.metadata.starts_with("retainedBytes="));
        assert!(!loss.delivery_failed);
        // The marker that carries it is a real queued batch of this lane.
        let marker = spool.at(0).unwrap().expect("the gap marker is queued");
        assert_eq!(marker.sequence, loss.gap_sequence);
        assert_eq!(marker.kind, "gap");
        assert!(marker.has_gap);
        assert_eq!(marker.gap_kind, "access_log");
        assert_eq!(marker.gap_reason, "unattributed_pre_assignment");
        // Only a real receipt marks the audit delivered.
        assert!(spool.loss_record(0).unwrap().is_some());
        spool.acknowledge(&receipt_for(&marker)).unwrap();
        assert!(
            spool.loss_record(0).unwrap().is_none(),
            "a durable receipt marks the loss delivered"
        );
        assert_eq!(spool.collapse_confirmed(256).unwrap(), 4);
        assert_eq!(spool.snapshot()["head"], batches[0].sequence + 5);
        assert_eq!(spool.snapshot()["ackedAhead"], 0);
        assert_eq!(spool.snapshot()["retainedBytes"], 0);
        let archived = spool.snapshot()["quarantineBytes"].as_u64().unwrap();
        assert!(archived > 0);
        assert_eq!(spool.budget.counter.used(), archived);
        assert_eq!(
            spool.quarantined_batch(batches[0].sequence).unwrap(),
            batches[0]
        );
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Quarantine, its audit, the gap marker and head advance are one commit.
    /// Reopening preserves the rejected payload even after the gap is delivered.
    #[test]
    fn quarantine_is_atomic_and_survives_reopen() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        for at in [100, 200] {
            spool
                .append("access_log", vec![at as u8], 0, false, at)
                .unwrap();
        }
        let head = spool.at(0).unwrap().unwrap();
        assert!(
            spool
                .quarantine_head(
                    head.sequence,
                    TerminalReason::InvalidArgument.gap_reason(),
                    "invalid telemetry envelope"
                )
                .unwrap()
        );
        // The head moves only together with the original envelope and audit.
        assert_eq!(spool.quarantined_batch(head.sequence).unwrap(), head);
        let snapshot = spool.snapshot();
        assert_eq!(snapshot["head"], head.sequence + 1);
        assert_eq!(snapshot["tail"], head.sequence + 3);
        assert_eq!(snapshot["pendingBatches"], 2);
        assert_eq!(snapshot["quarantinedBatches"], 1);
        assert_eq!(
            snapshot["lastQuarantineReason"],
            "terminal_invalid_argument"
        );
        assert_eq!(snapshot["journaledLosses"], 1);
        assert_eq!(spool.gap_bounds(), (100, 100, false));
        let loss = spool
            .loss_record(0)
            .unwrap()
            .expect("the loss is journaled");
        assert_eq!(loss.batch_id, head.batch_id);
        assert_eq!(loss.sequence, head.sequence);
        assert_eq!(loss.lane, "detail");
        assert_eq!(loss.kind, "access_log");
        assert_eq!(loss.event_time, 100);
        assert_eq!(loss.reason, "terminal_invalid_argument");
        assert_eq!(loss.error, "invalid telemetry envelope");
        assert_eq!(loss.gap_sequence, head.sequence + 2);
        let used = spool.budget.counter.used();
        drop(spool);

        // A restart restores the counters, the reason, the bounds, the audit and
        // the marker: nothing is re-emitted and nothing is forgotten.
        let spool = Spool::open(&path, false, MIB).unwrap();
        assert_eq!(spool.snapshot()["head"], head.sequence + 1);
        assert_eq!(spool.snapshot()["tail"], head.sequence + 3);
        assert_eq!(spool.snapshot()["quarantinedBatches"], 1);
        assert_eq!(
            spool.snapshot()["lastQuarantineReason"],
            "terminal_invalid_argument"
        );
        assert_eq!(spool.snapshot()["journaledLosses"], 1);
        assert_eq!(spool.gap_bounds(), (100, 100, false));
        assert_eq!(spool.budget.counter.used(), used);
        assert_eq!(spool.quarantined_batch(head.sequence).unwrap(), head);
        let loss = spool
            .loss_record(0)
            .unwrap()
            .expect("the audit survives the restart");
        assert_eq!(loss.gap_sequence, head.sequence + 2);
        assert!(!loss.delivery_failed);
        // The marker is reachable through the replay path and is the only gap.
        let (window, _) = spool.replay_window(0).unwrap();
        let marker = window.iter().find(|batch| batch.has_gap).expect("marker");
        assert_eq!(marker.sequence, loss.gap_sequence);
        assert_eq!(
            window.iter().filter(|batch| batch.has_gap).count(),
            1,
            "a restart must not duplicate the marker"
        );
        // Only a real receipt marks it delivered.
        assert!(spool.loss_record(0).unwrap().is_some());
        let surviving = window
            .iter()
            .find(|batch| !batch.has_gap)
            .expect("the batch behind the loss survives the restart");
        spool.acknowledge(&receipt_for(surviving)).unwrap();
        assert!(
            spool.loss_record(0).unwrap().is_some(),
            "a receipt for another batch must not mark the loss delivered"
        );
        spool.acknowledge(&receipt_for(marker)).unwrap();
        assert!(spool.loss_record(0).unwrap().is_none());
        // Both receipts empty the replay lane. The original rejected envelope
        // and its audit remain charged and recoverable after the gap receipt.
        let snapshot = spool.snapshot();
        assert_eq!(snapshot["head"], snapshot["tail"]);
        assert_eq!(snapshot["ackedAhead"], 0);
        assert_eq!(snapshot["pendingBatches"], 0);
        assert_eq!(snapshot["retainedBytes"], 0);
        assert_eq!(spool.collapse_confirmed(256).unwrap(), 0);
        let archived = snapshot["quarantineBytes"].as_u64().unwrap();
        assert!(archived > 0);
        assert_eq!(spool.budget.counter.used(), archived);
        assert_eq!(spool.quarantined_batch(head.sequence).unwrap(), head);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn quarantine_capacity_failure_keeps_original_head() {
        let (mut spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        spool
            .append("access_log", vec![1, 2, 3], 0, false, 100)
            .unwrap();
        let head = spool.front().unwrap().unwrap();
        let before = spool.snapshot();
        spool.budget.limit = before["retainedBytes"].as_u64().unwrap();
        let err = spool
            .quarantine_head(head.sequence, "terminal_invalid_argument", "invalid")
            .unwrap_err();
        assert!(is_capacity_exhausted(err.as_ref()));
        assert_eq!(spool.front().unwrap().unwrap(), head);
        assert_eq!(spool.snapshot()["head"], before["head"]);
        assert_eq!(spool.snapshot()["quarantineBytes"], 0);
        assert!(spool.loss_record(0).unwrap().is_none());
        assert!(spool.quarantined_batch(head.sequence).is_err());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-3: quarantining anything but the head is a no-op — no payload leaves,
    /// no audit is written, and the caller is told so.
    #[test]
    fn quarantine_of_a_non_head_batch_changes_nothing() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        for at in [100, 200] {
            spool
                .append("access_log", vec![at as u8], 0, false, at)
                .unwrap();
        }
        let before = spool.snapshot();
        assert!(
            !spool
                .quarantine_head(2, TerminalReason::InvalidArgument.gap_reason(), "nope")
                .unwrap()
        );
        assert_eq!(spool.snapshot(), before);
        assert!(spool.loss_record(0).unwrap().is_none());
        assert!(spool.front().unwrap().is_some());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-3: a gap marker the API refuses must not generate another marker for
    /// itself — the audit it carried is marked as a failed delivery instead.
    #[test]
    fn refused_gap_marker_does_not_recurse() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        spool.append("access_log", vec![1], 0, false, 100).unwrap();
        let head = spool.at(0).unwrap().unwrap();
        assert!(
            spool
                .quarantine_head(
                    head.sequence,
                    TerminalReason::InvalidArgument.gap_reason(),
                    "invalid telemetry envelope"
                )
                .unwrap()
        );
        let marker = spool.at(0).unwrap().expect("the marker is queued");
        assert!(marker.has_gap);
        let tail_before = spool.snapshot()["tail"].clone();
        // The API permanently refuses the marker itself.
        assert!(
            spool
                .quarantine_head(
                    marker.sequence,
                    TerminalReason::Unimplemented.gap_reason(),
                    "kind not served"
                )
                .unwrap()
        );
        assert_eq!(
            spool.snapshot()["tail"],
            tail_before,
            "a refused marker must not spawn another"
        );
        assert_eq!(spool.snapshot()["quarantinedBatches"], 2);
        let carried = spool
            .loss_record(0)
            .unwrap()
            .expect("the original audit stays");
        assert!(carried.delivery_failed, "the undeliverable audit is marked");
        assert_eq!(carried.reason, "terminal_invalid_argument");
        // The refusal itself is journaled, but without a marker of its own.
        let refusal = spool.loss_record(1).unwrap().unwrap();
        assert_eq!(refusal.gap_sequence, 0);
        assert_eq!(refusal.reason, "terminal_unimplemented");
        assert!(spool.at(0).unwrap().is_none());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-2: a retryable failure keeps the batch and still backs off.
    #[tokio::test]
    async fn retryable_head_failure_backs_off_and_retains_the_batch() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        let spool = Arc::new(spool);
        spool.append("access_log", vec![1], 0, false, 100).unwrap();
        let head = spool.at(0).unwrap().unwrap();
        let uploads = vec![(
            head.batch_id.clone(),
            head.sequence,
            tokio::spawn(async {
                Err(anyhow::Error::from(tonic::Status::unavailable(
                    "assignment unavailable",
                )))
            }),
        )];
        let outcome = upload_window(spool.clone(), uploads).await;
        assert_eq!(outcome.confirmed, 0);
        assert!(outcome.quarantined.is_none(), "nothing may be dropped");
        assert_eq!(
            outcome.failure.as_ref().map(|(class, _)| *class),
            Some(UploadClass::Retryable)
        );
        assert_eq!(outcome.action(), WindowAction::Backoff);
        // The payload is still there, and the lane still counts it as pending.
        assert_eq!(spool.front().unwrap().unwrap().batch_id, head.batch_id);
        let snapshot = spool.snapshot();
        assert_eq!(snapshot["pendingBatches"], 1);
        assert_eq!(snapshot["quarantinedBatches"], 0);
        assert_eq!(snapshot["lastQuarantineReason"], "");
        assert_eq!(snapshot["retryableFailures"], 1);
        assert_eq!(snapshot["terminalFailures"], 0);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-4: the byte cap splits a batch the count caps would have let through,
    /// and the cap counts the *serialized* batch, envelope included.
    #[test]
    fn byte_cap_splits_batches_under_the_wire_limit() {
        const {
            assert!(MAX_BATCH_ENCODED_BYTES < RPC_NODE_STREAM_MESSAGE_BYTES);
            assert!(
                RPC_NODE_STREAM_MESSAGE_BYTES - MAX_BATCH_ENCODED_BYTES >= MIB as usize,
                "the cap must leave a real margin under the RPC message limit"
            );
        }
        let record = |len: usize| pb::NodeLog {
            description: "x".repeat(len),
            ..Default::default()
        };
        let budget = payload_budget("node_log");
        assert!(
            budget < MAX_BATCH_ENCODED_BYTES,
            "the envelope costs something"
        );
        let cost = record_cost(&record(MIB as usize));
        assert!((MIB as usize..2 * MIB as usize).contains(&cost));
        let per_batch = budget / cost;
        assert!(
            per_batch > 1,
            "the case needs more than one record per batch"
        );

        // More records than one batch may hold: the buffer must come out as
        // several batches, and each finished batch must fit the wire.
        let records = vec![record(MIB as usize); per_batch + 2];
        let (batches, oversize) = split_records(records, budget);
        assert!(oversize.is_empty());
        assert_eq!(batches.len(), 2);
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), per_batch + 2);
        for node_logs in &batches {
            let request = pb::CreateNodeLogsRequest {
                node_logs: node_logs.clone(),
            };
            let batch = pb::TelemetryBatch {
                schema_version: 1,
                batch_id: format!("{}:{:020}", "0".repeat(36), u64::MAX),
                boot_id: "0".repeat(36),
                sequence: u64::MAX,
                kind: "node_log".into(),
                payload: request.encode_to_vec(),
                occurred_at: i64::MIN,
                window_start: i64::MIN,
                source_boot_id: "0".repeat(36),
                ..Default::default()
            };
            assert!(
                batch.encoded_len() <= MAX_BATCH_ENCODED_BYTES,
                "a produced batch must fit the wire: {}",
                batch.encoded_len()
            );
        }

        // One record larger than a whole batch: returned on its own for
        // journaling, while the records around it still form a batch.
        let (batches, oversize) = split_records(
            vec![
                record(MIB as usize),
                record(budget + 1),
                record(MIB as usize),
            ],
            budget,
        );
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
        assert_eq!(oversize.len(), 1);
        assert!(record_cost(&oversize[0]) > budget);
    }

    /// P0-4: a record too large for any batch is journaled durably, with its own
    /// gap marker, instead of producing an empty batch or a silent error.
    #[test]
    fn oversize_record_is_journaled_not_silently_dropped() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        let budget = payload_budget("node_log");
        let (batches, oversize) = split_records(
            vec![
                pb::NodeLog {
                    description: "x".repeat(MIB as usize),
                    ..Default::default()
                },
                pb::NodeLog {
                    description: "x".repeat(budget + 1),
                    ..Default::default()
                },
            ],
            budget,
        );
        assert_eq!(batches.len(), 1, "the records that fit still form a batch");
        assert_eq!(oversize.len(), 1);
        assert_eq!(spool.snapshot()["oversizeDropped"], 0);
        spool
            .journal_oversize("node_log", record_cost(&oversize[0]))
            .unwrap();
        assert_eq!(spool.snapshot()["oversizeDropped"], 1);
        assert_eq!(spool.snapshot()["journaledLosses"], 1);
        let loss = spool
            .loss_record(0)
            .unwrap()
            .expect("the loss is journaled");
        assert_eq!(loss.batch_id, "", "an oversize record never had a batch");
        assert_eq!(loss.kind, "node_log");
        assert_eq!(loss.lane, "detail");
        assert!(loss.reason.starts_with(OVERSIZE_REASON), "{}", loss.reason);
        assert!(loss.metadata.contains("encodedBytes="));
        assert!(loss.event_time > 0);
        // The journal produced a gap marker, not an empty data batch.
        let marker = spool.at(0).unwrap().expect("the gap marker is queued");
        assert_eq!(marker.kind, "gap");
        assert!(marker.has_gap);
        assert_eq!(marker.gap_kind, "node_log");
        assert_eq!(marker.sequence, loss.gap_sequence);
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        // Reopening keeps the audit and its counter.
        let spool = Spool::open(&path, false, MIB).unwrap();
        assert_eq!(spool.snapshot()["oversizeDropped"], 1);
        assert!(spool.loss_record(0).unwrap().is_some());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-4: the cap is enforced on the finished message, envelope included, for
    /// every kind — a producer that cannot split gets a typed refusal instead of
    /// a batch the RPC layer would reject, and nothing half-written.
    #[test]
    fn append_prepared_refuses_a_batch_over_the_wire_cap() {
        let (spool, path) = spool_with_disk(false, 4 * GIB, GIB, GIB);
        // A payload that just fits the observation budget still lands...
        let budget = payload_budget("probe_observation");
        let payload = pb::ProbeObservationBatch {
            observations: vec![pb::ProbeObservation {
                error: "e".repeat(budget - 1024),
                ..Default::default()
            }],
        }
        .encode_to_vec();
        assert!(payload.len() <= budget);
        spool
            .append("probe_observation", payload, 0, false, 1)
            .unwrap();
        // ...and a payload the envelope can no longer absorb is refused by name,
        // for a kind no splitter knows about.
        let err = spool
            .append(
                "node_metric",
                vec![0u8; MAX_BATCH_ENCODED_BYTES],
                0,
                false,
                2,
            )
            .expect_err("a batch over the message budget must be refused");
        assert!(is_batch_too_large(err.as_ref()), "{err}");
        assert!(err.to_string().contains("reason=batch_too_large"), "{err}");
        assert!(!is_capacity_exhausted(err.as_ref()));
        // Nothing was written: the queue still holds only the fitting batch.
        assert_eq!(spool.snapshot()["tail"], 2);
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P0-4: a probe observation batch is split to the wire cap before it reaches
    /// the spool, and an observation too large for any batch is returned for
    /// journaling rather than shipped as an empty batch.
    #[test]
    fn observation_payloads_are_split_under_the_wire_cap() {
        let budget = payload_budget("probe_observation");
        let fat = |len: usize| pb::ProbeObservation {
            error: "e".repeat(len),
            ..Default::default()
        };
        let cost = record_cost(&fat(MIB as usize));
        assert!((MIB as usize..2 * MIB as usize).contains(&cost));
        let per_batch = budget / cost;
        assert!(per_batch >= 2, "the case needs more than one per batch");

        let records = (0..per_batch + 1).map(|_| fat(MIB as usize)).collect();
        let (batches, oversize) = split_records(records, budget);
        assert!(oversize.is_empty());
        assert_eq!(
            batches.len(),
            2,
            "the count cap alone would have shipped one"
        );
        for observations in &batches {
            let batch = pb::TelemetryBatch {
                schema_version: 1,
                kind: "probe_observation".into(),
                payload: pb::ProbeObservationBatch {
                    observations: observations.clone(),
                }
                .encode_to_vec(),
                occurred_at: 1,
                ..Default::default()
            };
            assert!(
                batch.encoded_len() <= MAX_BATCH_ENCODED_BYTES,
                "a produced batch must fit the wire: {}",
                batch.encoded_len()
            );
        }
        // Every observation is carried exactly once.
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), per_batch + 1);

        // One observation over the whole cap: returned for journaling, never
        // padded into an empty batch.
        let (batches, oversize) = split_records(vec![fat(budget + 1)], budget);
        assert!(
            batches.is_empty(),
            "an oversize observation must not become an empty batch"
        );
        assert_eq!(oversize.len(), 1);
        assert!(record_cost(&oversize[0]) > budget);
    }

    /// A failed chunk must return exactly the records the spool did not
    /// persist: committed chunks and journaled oversize records stay out, and
    /// every remaining record appears once.
    #[tokio::test]
    async fn probe_enqueue_failure_returns_only_unpersisted_records() {
        let budget = payload_budget("probe_observation");
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let noisy = |len: usize| -> String {
            incompressible(len)
                .into_iter()
                .map(|byte| char::from(alphabet[(byte & 63) as usize]))
                .collect()
        };
        let records: Vec<_> = (0..60)
            .map(|index| pb::ProbeObservation {
                target_id: format!("pt_{index}"),
                error: noisy(250_000),
                ..Default::default()
            })
            .collect();
        let (batches, oversize) = split_records(records.clone(), budget);
        assert!(oversize.is_empty());
        assert!(batches.len() >= 2, "the case needs a later chunk to fail");
        let committed = batches[0].len();
        // Size the lane so the first chunk fits with the envelope, while any
        // later chunk cannot: the failure is guaranteed to land mid-batch.
        let first = pb::ProbeObservationBatch {
            observations: batches[0].clone(),
        }
        .encode_to_vec();
        let first_compressed = zstd::encode_all(first.as_slice(), 3).unwrap().len() as u64;
        let limit = first_compressed + 8 * 1024;

        let path = std::env::temp_dir().join(format!("telemetry-probe-{}", uuid::Uuid::new_v4()));
        let spool = Arc::new(Spool::open(&path, false, limit).unwrap());
        let error = enqueue_probe_observations_on(spool.clone(), records, 0)
            .await
            .expect_err("the second chunk exceeds the spool budget");
        assert_eq!(error.lost(), 0);
        let unsent = error.into_unsent();
        assert_eq!(
            unsent.len(),
            60 - committed,
            "only the chunks after the committed one are unsent"
        );
        for (offset, observation) in unsent.iter().enumerate() {
            assert_eq!(
                observation.target_id,
                format!("pt_{}", committed + offset),
                "unsent records must be the exact tail, in order"
            );
        }
        // The committed chunk is durable; the failed one is not.
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// The metric reporter must retain exactly the rows a failed chunk did not
    /// persist; committed chunks are not replayed.
    #[tokio::test]
    async fn metric_enqueue_failure_returns_only_unpersisted_rows() {
        let budget = payload_budget("metric_stat");
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let noisy = |len: usize| -> String {
            incompressible(len)
                .into_iter()
                .map(|byte| char::from(alphabet[(byte & 63) as usize]))
                .collect()
        };
        let rows: Vec<_> = (0..60)
            .map(|index| pb::UploadingMetricStat {
                keys: vec![format!("k_{index}"), noisy(250_000)],
                value: index as f32,
                ..Default::default()
            })
            .collect();
        let header = |metric_stats| pb::UploadMetricStatsRequest {
            server_id: 7,
            time: "202610281200".into(),
            count: 60,
            total: 1770.0,
            version: 3,
            item_id: 9,
            metric_stats,
            ..Default::default()
        };
        let row_budget = budget
            .saturating_sub(header(Vec::new()).encoded_len())
            .max(1);
        let (batches, oversize) = split_records(rows.clone(), row_budget);
        assert!(oversize.is_empty());
        assert!(batches.len() >= 2, "the case needs a later chunk to fail");
        let committed = batches[0].len();
        let first = header(batches[0].clone()).encode_to_vec();
        let first_compressed = zstd::encode_all(first.as_slice(), 3).unwrap().len() as u64;
        let limit = first_compressed + 8 * 1024;

        let path = std::env::temp_dir().join(format!("telemetry-metric-{}", uuid::Uuid::new_v4()));
        let spool = Arc::new(Spool::open(&path, false, limit).unwrap());
        let error = enqueue_metric_stat_on(spool.clone(), header(rows))
            .await
            .expect_err("the second chunk exceeds the spool budget");
        assert_eq!(error.lost(), 0);
        let unsent = error.into_unsent();
        assert_eq!(unsent.server_id, 7);
        assert_eq!(unsent.item_id, 9);
        assert_eq!(unsent.metric_stats.len(), 60 - committed);
        for (offset, row) in unsent.metric_stats.iter().enumerate() {
            assert_eq!(
                row.keys[0],
                format!("k_{}", committed + offset),
                "unsent rows must be the exact tail, in order"
            );
        }
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Daily server rows keep the same exact-ownership rule: only the chunks
    /// after the committed one are returned for the next window.
    #[tokio::test]
    async fn daily_enqueue_failure_returns_only_unpersisted_rows() {
        let budget = payload_budget("daily");
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let noisy = |len: usize| -> String {
            incompressible(len)
                .into_iter()
                .map(|byte| char::from(alphabet[(byte & 63) as usize]))
                .collect()
        };
        let rows: Vec<_> = (0..60)
            .map(|index| pb::ServerDailyStat {
                server_id: 7,
                time_from: format!("{index}:{}", noisy(250_000)),
                ..Default::default()
            })
            .collect();
        let (batches, oversize) = split_records(rows.clone(), budget);
        assert!(oversize.is_empty());
        assert!(batches.len() >= 2, "the case needs a later chunk to fail");
        let committed = batches[0].len();
        let first = daily_request(batches[0].clone(), Vec::new()).encode_to_vec();
        let first_compressed = zstd::encode_all(first.as_slice(), 3).unwrap().len() as u64;
        let limit = first_compressed + 8 * 1024;

        let path = std::env::temp_dir().join(format!("telemetry-daily-{}", uuid::Uuid::new_v4()));
        let spool = Arc::new(Spool::open(&path, false, limit).unwrap());
        let error = enqueue_daily_stats_on(spool.clone(), daily_request(rows, Vec::new()))
            .await
            .expect_err("the second chunk exceeds the spool budget");
        assert_eq!(error.lost(), 0);
        let unsent = error.into_unsent();
        assert!(unsent.domain_stats.is_empty());
        assert_eq!(unsent.stats.len(), 60 - committed);
        for (offset, row) in unsent.stats.iter().enumerate() {
            assert_eq!(row.server_id, 7);
            assert!(
                row.time_from
                    .starts_with(&format!("{}:", committed + offset)),
                "unsent rows must be the exact tail, in order"
            );
        }
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// P1-2: the node status exposes the lanes' queue position, the quarantine
    /// and per-class counters, and the reason of the last gap — while the
    /// fields the API already reads stay in place.
    #[test]
    fn status_snapshot_exposes_head_tail_quarantine_and_gap_counters() {
        let (core, core_path) = spool_with_disk(true, MIB, GIB, GIB);
        let (detail, detail_path) = spool_with_disk(false, MIB, GIB, GIB);
        for _ in 0..3 {
            detail.append("access_log", vec![1], 0, false, 100).unwrap();
        }
        detail
            .quarantine_head(
                1,
                TerminalReason::PreAssignment.gap_reason(),
                "assignment missing",
            )
            .unwrap();

        let status = snapshot_of(&core, &detail);
        assert_eq!(status["detail"]["head"], 2);
        assert_eq!(status["detail"]["tail"], 5);
        assert_eq!(status["detail"]["pendingBatches"], 3);
        assert_eq!(status["detail"]["quarantinedBatches"], 1);
        assert_eq!(
            status["detail"]["lastQuarantineReason"],
            "unattributed_pre_assignment"
        );
        assert_eq!(status["detail"]["journaledLosses"], 1);
        assert_eq!(status["detail"]["retryableFailures"], 0);
        assert_eq!(status["detail"]["terminalFailures"], 0);
        assert_eq!(status["core"]["head"], 1);
        assert_eq!(status["core"]["tail"], 1);
        assert_eq!(status["core"]["quarantinedBatches"], 0);
        assert_eq!(status["core"]["journaledLosses"], 0);
        assert!(status.get("lastGapReason").is_some());
        assert!(status.get("oversizeDroppedObservations").is_some());
        // The pre-existing fields must not move or disappear.
        for field in ["gapStart", "gapEnd", "gapHasCore"] {
            assert!(status.get(field).is_some(), "{field} must stay reported");
        }
        for field in [
            "pendingBatches",
            "pendingBytes",
            "retainedBytes",
            "remainingBytes",
            "capacityLevel",
            "oldestAt",
            "exhaustEtaSecs",
            "lastFailureAt",
        ] {
            assert!(
                status["detail"].get(field).is_some(),
                "{field} must stay reported"
            );
        }
        drop(core);
        drop(detail);
        std::fs::remove_dir_all(core_path).unwrap();
        std::fs::remove_dir_all(detail_path).unwrap();
    }

    #[test]
    fn replay_cursor_reaches_batches_beyond_a_failed_window() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        for _ in 0..REPLAY_CONCURRENCY + 1 {
            spool.append("node_log", vec![], 0, false, 100).unwrap();
        }
        let (first, cursor) = spool.replay_window(0).unwrap();
        assert_eq!(first.len(), REPLAY_CONCURRENCY);
        let (later, cursor) = spool.replay_window(cursor).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].sequence, REPLAY_CONCURRENCY as u64 + 1);
        let (retry, _) = spool.replay_window(cursor).unwrap();
        assert_eq!(retry[0].batch_id, first[0].batch_id);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replay_reports_missing_unconfirmed_payloads() {
        let (spool, path) = spool_with_disk(false, MIB, GIB, GIB);
        spool.append("node_log", vec![1], 0, false, 100).unwrap();
        let txn = spool.bucket.begin().unwrap();
        txn.del(b"batch:00000000000000000001").unwrap();
        txn.commit().unwrap();
        assert!(spool.front().is_err());
        assert!(spool.replay_window(0).is_err());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Verbatim copy of the pre-fix policy, kept so the regression case fails
    /// (capacity == 0) if anyone restores `max(total / 50, 1 GiB)` without
    /// clamping the reserve against what is actually available.
    fn pre_fix_capacity(total: u64, available: u64, core: bool) -> u64 {
        let reserve = (total / 50).max(GIB);
        let core_reserve = if core { 0 } else { 256 * MIB };
        available.saturating_sub(reserve + core_reserve)
    }

    // The live acceptance scenario: a bind-mounted host disk of 240 GiB with
    // 3.8 GiB free. The pre-fix reserve was 4.6 GiB, so every lane went to
    // zero and stayed there even though 3.8 GiB was free.
    #[test]
    fn tight_disk_keeps_a_positive_adaptive_budget() {
        let total = TIGHT_TOTAL;
        let available = TIGHT_AVAILABLE;

        // Negative control: the old formula closes the spool for both lanes.
        assert_eq!(pre_fix_capacity(total, available, true), 0);
        assert_eq!(pre_fix_capacity(total, available, false), 0);
        // The old ceiling was a 1 GiB floor, far larger than the ~1 GiB quarter
        // of free space the fix is willing to hold back.
        assert!((total / 50).max(GIB) > available / RESERVE_AVAILABLE_DIVISOR);

        // The fix: enough room for both lanes to keep working, while the disk
        // keeps a reserve proportional to what is actually free.
        let core = lane_capacity(true, total, available, 0, 0);
        let detail = lane_capacity(false, total, available, 0, 0);
        assert!(
            core > 0 && detail > 0,
            "both lanes must keep a positive budget: core={core} detail={detail}"
        );

        // Never more than the free space, and detail's own pool is never
        // smaller than core could leave it: the reserve is one pool, not one
        // per lane. Two independent carves would let core + detail exceed the
        // disk (2.69 + 3.22 > 4.30 GiB in this very scenario).
        assert!(core <= available);
        assert!(detail <= available);
        assert_eq!(detail, available_bytes(total, available, 0));
        assert_eq!(
            core,
            available_bytes(total, available, core_reserved_bytes(total, available))
        );
        assert!(core < detail, "core must reserve a slice for detail");
    }

    // The reserve must adapt with available space instead of tracking total
    // size, and a full disk must still be a hard stop.
    #[test]
    fn reserve_adapts_to_available_space_and_stops_at_zero_free() {
        // The 1/50 share of a 240 GiB disk (4.8 GiB) is what caps the reserve
        // here, because it is far below max(FLOOR, available / 4).
        let expected_reserve =
            |available: u64| (TIGHT_TOTAL / RESERVE_RATIO).min((available / 4).max(RESERVE_FLOOR));
        // Large free space: the total/50 share is the binding ceiling, so
        // 2 GiB of a 100 GiB disk stays reserved.
        assert_eq!(available_bytes(100 * GIB, 90 * GIB, 0), 90 * GIB - 2 * GIB);
        // Tight free space: the ceiling is clamped to a quarter of free space.
        assert_eq!(
            available_bytes(TIGHT_TOTAL, TIGHT_AVAILABLE, 0),
            TIGHT_AVAILABLE - expected_reserve(TIGHT_AVAILABLE)
        );
        // No free space: the lane is closed, never over-quota into zero, and
        // never a panic from an inverted clamp on a full disk.
        assert_eq!(available_bytes(TIGHT_TOTAL, 0, 0), 0);
        assert_eq!(available_bytes(TIGHT_TOTAL, 1, 0), 0);
        assert_eq!(lane_capacity(true, TIGHT_TOTAL, 0, 0, 0), 0);
        assert_eq!(lane_capacity(false, TIGHT_TOTAL, 0, 0, 0), 0);
    }

    // A real spool must reach the same decision through `append`, so the fix is
    // not just arithmetic sitting beside the code path that runs in production.
    #[test]
    fn append_uses_the_adaptive_budget_on_a_tight_disk() {
        let total = TIGHT_TOTAL;
        let available = TIGHT_AVAILABLE;
        assert_eq!(pre_fix_capacity(total, available, true), 0);
        let (spool, path) = spool_with_disk(true, 0, total, available);
        // The decision path itself reports a usable budget, not zero.
        let budget = spool.capacity();
        assert!(budget > 0, "tight disk must not close the spool");
        assert_eq!(
            budget,
            available_bytes(total, available, core_reserved_bytes(total, available))
        );
        // A restrained batch lands on a disk the pre-fix policy had closed.
        spool
            .append("node_metric", incompressible(4 * MIB as usize), 0, false, 1)
            .unwrap();
        // An explicitly limited queue still enforces its ceiling: the fix does
        // not turn capacity checking off, it changes what capacity is.
        let (limited, limited_path) = spool_with_disk(true, MIB, total, available);
        limited
            .append("node_metric", incompressible(64 * 1024), 0, false, 1)
            .unwrap();
        assert!(
            limited
                .append("node_metric", incompressible(2 * MIB as usize), 0, false, 2)
                .is_err(),
            "a batch past the queue ceiling must be refused"
        );
        drop(spool);
        drop(limited);
        std::fs::remove_dir_all(path).unwrap();
        std::fs::remove_dir_all(limited_path).unwrap();
    }

    // Requirement: detail keeps a landing place once core has filled its own
    // budget, and core is charged for the slice set aside for detail.
    #[test]
    fn detail_still_enqueues_after_core_fills_its_budget() {
        let total = 20 * GIB;
        let available = total / 2;
        let (core, core_path) = spool_with_disk(true, MIB, total, available);
        let (detail, detail_path) = spool_with_disk(false, 0, total, available);
        let detail = detail.with_peer(core.budget.counter.clone());

        let core_budget = core.capacity();
        let detail_budget = detail.capacity();
        assert_eq!(core_budget, MIB);
        // Detail's budget is the pool itself: detail is the fallback lane and
        // is not charged core's peer reserve.
        assert_eq!(detail_budget, available_bytes(total, available, 0));
        // Isolation is by construction, not by luck: core's budget leaves out
        // the detail carve, and detail's budget ignores core's ceiling.
        assert!(detail_budget > 0);
        assert!(core_budget < detail_budget);

        // Fill core to its ceiling.
        let mut appended = 0u32;
        while core
            .append("node_metric", vec![7; 64 * 1024], 0, false, 1)
            .is_ok()
        {
            appended += 1;
        }
        assert!(appended > 0);
        // The queue stops just short of its ceiling rather than overrunning it.
        assert!(core.budget.counter.used() > 0);
        assert!(core.budget.counter.used() <= MIB);
        // Core is done for this window; it cannot spend detail's slice.
        assert!(
            core.append("node_metric", vec![7; 64 * 1024], 0, false, 1)
                .is_err()
        );

        // Detail still lands its observation, which is the starvation the
        // inverted `core_reserve` (charged to detail instead of core) caused.
        detail
            .append("probe_observation", vec![9; 8 * 1024], 0, false, 3)
            .unwrap();
        assert_eq!(detail.front().unwrap().unwrap().kind, "probe_observation");

        drop(core);
        drop(detail);
        std::fs::remove_dir_all(core_path).unwrap();
        std::fs::remove_dir_all(detail_path).unwrap();
    }

    // The invariants must hold across the whole parameter space, not only at the
    // scenario the acceptance run happened to hit. A completely full disk must
    // return zero capacity rather than panicking inside a clamp.
    #[test]
    fn capacity_invariants_hold_across_disk_sizes() {
        for total in [64 * MIB, GIB, 20 * GIB, 240 * GIB, 8 * GIB * 1024] {
            let mut available = total;
            while available > 0 {
                let core = lane_capacity(true, total, available, 0, 0);
                let detail = lane_capacity(false, total, available, 0, 0);
                for (lane, capacity, is_core) in [("core", core, true), ("detail", detail, false)] {
                    assert!(
                        capacity <= available,
                        "{lane} over-quota: {total}/{available}"
                    );
                    let released = available - capacity;
                    let base = (total / RESERVE_RATIO)
                        .min((available / RESERVE_AVAILABLE_DIVISOR).max(RESERVE_FLOOR));
                    let peer = if is_core {
                        core_reserved_bytes(total, available)
                    } else {
                        0
                    };
                    assert!(
                        released <= base + peer,
                        "{lane} released {released} exceeds reserve {base} + peer {peer}"
                    );
                }
                // The reserve is capped by this disk's 1/50 share, so the spool
                // cannot reserve more than the disk can spare.
                let reserve = available - available_bytes(total, available, 0);
                assert!(reserve <= total / RESERVE_RATIO);
                assert!(reserve <= available);
                // Keep the sweep strictly decreasing: once `available` is
                // smaller than 64 the step would be zero and the loop would
                // never terminate.
                let step = (available / 64).max(1);
                available = available.saturating_sub(step);
            }
            // A full disk is a closed lane, never a panic.
            assert_eq!(available_bytes(total, 0, 0), 0);
            assert_eq!(lane_capacity(true, total, 0, 0, 0), 0);
            assert_eq!(lane_capacity(false, total, 0, 0, 0), 0);
        }
    }

    // Requirement: a capacity refusal is identifiable, and is not every refusal.
    #[test]
    fn capacity_refusal_is_distinguishable_from_other_failures() {
        let (spool, path) = spool_with_disk(true, MIB, TIGHT_TOTAL, TIGHT_AVAILABLE);
        let err = spool
            .append("node_metric", incompressible(2 * MIB as usize), 0, false, 1)
            .expect_err("batch larger than the lane budget must be refused");
        let rendered = err.to_string();
        assert!(
            rendered.contains("reason=spool_capacity"),
            "capacity refusal must carry its own reason code: {rendered}"
        );
        assert!(
            rendered.contains("lane=core"),
            "capacity refusal must name the lane: {rendered}"
        );
        assert!(is_capacity_exhausted(err.as_ref()));
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
        // An unrelated spool-level failure is not classified as capacity.
        let other = std::io::Error::other("telemetry spool: not found");
        assert!(!is_capacity_exhausted(&other));
    }

    // Mirrors probe.rs: the Edge executor builds the batch from the pushed plan
    // identity and hands it to telemetry::enqueue_probe_observations.
    #[test]
    fn probe_observation_batch_reaches_the_spool_with_plan_identity() {
        let plan: crate::config_models::ProbePlan = serde_json::from_str(
            r#"{"enabled":true,"providers":[{"id":"pp_q","kind":"zstaticcdn","intervalSeconds":60,"timeoutMs":3000,"targets":[{"id":"pt_q","kind":"tcp","checkId":"probe:tcp:0123456789abcdef","clusterId":"ec_q","targetType":"host","host":"203.0.113.7","port":443,"ipFamily":"v4"}]}]}"#,
        )
        .unwrap();
        let target = &plan.providers[0].targets[0];
        let observation = pb::ProbeObservation {
            target_id: target.id.clone(),
            provider_id: plan.providers[0].id.clone(),
            kind: target.kind.clone(),
            check_id: target.check_id.clone(),
            cluster_id: target.cluster_id.clone(),
            host: target.host.clone(),
            port: u32::from(target.port),
            ip_family: target.ip_family.clone(),
            success: true,
            latency_ms: 18.5,
            sent: 3,
            received: 3,
            event_time: "2026-10-14 00:00:00.000".into(),
            ..Default::default()
        };
        let batch = pb::ProbeObservationBatch {
            observations: vec![observation],
        };
        let path = std::env::temp_dir().join(format!("telemetry-probe-{}", uuid::Uuid::new_v4()));
        let spool = Spool::open(&path, false, 1 << 20).unwrap();
        spool
            .append(
                "probe_observation",
                prost::Message::encode_to_vec(&batch),
                0,
                false,
                1,
            )
            .unwrap();
        let queued = spool.front().unwrap().expect("queued probe batch");
        assert_eq!(queued.kind, "probe_observation");
        let decoded =
            <pb::ProbeObservationBatch as prost::Message>::decode(queued.payload.as_slice())
                .unwrap();
        let row = &decoded.observations[0];
        assert_eq!(row.check_id, "probe:tcp:0123456789abcdef");
        assert_eq!(row.cluster_id, "ec_q");
        assert_eq!(row.kind, "tcp");
        assert_eq!(row.sent, 3);
        assert!(row.success);
    }

    #[test]
    fn restart_preserves_batch_until_matching_receipt() {
        let path = std::env::temp_dir().join(format!("telemetry-{}", uuid::Uuid::new_v4()));
        let id;
        {
            let spool = Spool::open(&path, true, 1 << 20).unwrap();
            id = spool.append("node_metric", vec![], 0, false, 1).unwrap();
        }
        let spool = Spool::open(&path, true, 1 << 20).unwrap();
        let b = spool.front().unwrap().unwrap();
        assert_eq!(b.batch_id, id);
        assert!(
            spool
                .acknowledge(&pb::TelemetryReceipt {
                    batch_id: "wrong".into(),
                    sequence: b.sequence,
                    backend: "direct".into(),
                    durable_at: 2,
                })
                .is_err()
        );
        assert!(spool.front().unwrap().is_some());
        spool
            .acknowledge(&pb::TelemetryReceipt {
                batch_id: id,
                sequence: b.sequence,
                backend: "direct".into(),
                durable_at: 2,
            })
            .unwrap();
        assert!(spool.front().unwrap().is_none());
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn producer_handoff_commits_queue_and_cursor_together() {
        let path = std::env::temp_dir().join(format!("telemetry-handoff-{}", uuid::Uuid::new_v4()));
        {
            let mut spool = Spool::open(&path, true, 1 << 20).unwrap();
            spool.save_state("handoff", b"pending").unwrap();
            let refused = pb::TelemetryBatch {
                kind: "bandwidth".into(),
                payload: vec![0; MAX_BATCH_ENCODED_BYTES],
                ..Default::default()
            };
            assert!(
                spool
                    .append_prepared_with_state(refused, Some(("handoff", b"advanced")))
                    .is_err()
            );
            assert!(spool.front().unwrap().is_none());
            assert_eq!(
                spool.load_state("handoff").unwrap().as_deref(),
                Some(b"pending".as_slice())
            );
            spool.budget.limit = spool.meta.lock().bytes + 1;
            let capacity_error = spool
                .append_prepared_with_state(
                    pb::TelemetryBatch {
                        kind: "bandwidth".into(),
                        payload: vec![41],
                        ..Default::default()
                    },
                    Some(("handoff", b"advanced")),
                )
                .unwrap_err();
            assert!(is_capacity_exhausted(capacity_error.as_ref()));
            assert!(spool.front().unwrap().is_none());
            assert_eq!(
                spool.load_state("handoff").unwrap().as_deref(),
                Some(b"pending".as_slice())
            );
            spool.budget.limit = 1 << 20;
            spool
                .append_prepared_with_state(
                    pb::TelemetryBatch {
                        kind: "bandwidth".into(),
                        payload: vec![42],
                        ..Default::default()
                    },
                    Some(("handoff", b"advanced")),
                )
                .unwrap();
        }
        let spool = Spool::open(&path, true, 1 << 20).unwrap();
        assert_eq!(
            spool.load_state("handoff").unwrap().as_deref(),
            Some(b"advanced".as_slice())
        );
        assert_eq!(spool.front().unwrap().unwrap().payload, vec![42]);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn rebuilt_spool_rejects_receipts_from_the_previous_data_generation() {
        let path = std::env::temp_dir().join(format!("telemetry-reset-{}", uuid::Uuid::new_v4()));
        let old_batch = {
            let spool = Spool::open(&path, true, 1 << 20).unwrap();
            spool.append("node_metric", vec![1], 0, false, 1).unwrap();
            spool.front().unwrap().unwrap()
        };
        std::fs::remove_dir_all(&path).unwrap();
        let spool = Spool::open(&path, true, 1 << 20).unwrap();
        spool.append("node_metric", vec![2], 0, false, 2).unwrap();
        let new_batch = spool.front().unwrap().unwrap();
        assert_eq!(new_batch.sequence, old_batch.sequence);
        assert_ne!(new_batch.boot_id, old_batch.boot_id);
        assert_ne!(new_batch.batch_id, old_batch.batch_id);
        assert!(
            spool
                .acknowledge(&pb::TelemetryReceipt {
                    batch_id: old_batch.batch_id,
                    sequence: old_batch.sequence,
                    backend: "direct".into(),
                    durable_at: 3,
                })
                .is_err()
        );
        assert_eq!(spool.front().unwrap().unwrap().payload, vec![2]);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn no_age_expiration_and_no_overwrite_at_capacity() {
        let path = std::env::temp_dir().join(format!("telemetry-{}", uuid::Uuid::new_v4()));
        let spool = Spool::open(&path, true, 200).unwrap();
        spool.append("node_metric", vec![], 0, false, 1).unwrap();
        let first = spool.front().unwrap().unwrap().batch_id;
        for _ in 0..100 {
            if spool
                .append("node_metric", vec![1; 100], 0, false, 2)
                .is_err()
            {
                break;
            }
        }
        assert_eq!(spool.front().unwrap().unwrap().batch_id, first);
        assert_eq!(spool.front().unwrap().unwrap().occurred_at, 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn capacity_escalates() {
        assert_eq!(capacity_level(80, 20, 100), 80);
        assert_eq!(capacity_level(90, 10, 100), 90);
        assert_eq!(capacity_level(97, 3, 100), 97);
    }
}
