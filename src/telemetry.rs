//! Durable producer queues. API memory is not a delivery acknowledgement.
use crate::{api_config::ApiConfig, paths::NodePaths, pb, rpc::client::SharedRpcClient};
use mace::{Bucket, BucketOptions, Mace, OpCode, Options};
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
        }
    }
}
pub struct Spool {
    _db: Mace,
    bucket: Bucket,
    meta: Mutex<Meta>,
    path: PathBuf,
    budget: LaneBudget,
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
    let reserve = (total / RESERVE_RATIO)
        .min((available / RESERVE_AVAILABLE_DIVISOR).max(RESERVE_FLOOR));
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
    anyhow::anyhow!("telemetry spool: {e:?}")
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
        }
    }
}
impl std::error::Error for SpoolError {}
/// True when a spool error was a capacity rejection. Callers keep their own
/// drop/retain policy; this only classifies the cause.
pub fn is_capacity_exhausted(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(err) = current {
        if err.downcast_ref::<SpoolError>().is_some() {
            return true;
        }
        current = err.source();
    }
    false
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
        let bucket = match db.get_bucket("queue") {
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
        counter.add_used(meta.bytes);
        Ok(Self {
            _db: db,
            bucket,
            meta: Mutex::new(meta),
            path: path.to_owned(),
            budget: LaneBudget { limit, core, counter },
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
            self.budget
                .counter
                .reserved_for_peer(self.budget.core),
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
    pub fn append_prepared(&self, mut batch: pb::TelemetryBatch) -> anyhow::Result<String> {
        let mut meta = self.meta.lock();
        batch.schema_version = 1;
        batch.batch_id = format!("{}:{}", meta.boot, meta.tail);
        batch.boot_id = meta.boot.clone();
        batch.sequence = meta.tail;
        if batch.source_boot_id.is_empty() {
            batch.source_boot_id = PROCESS_BOOT.clone();
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
            && meta.bytes.saturating_add(required) > self.budget.limit;
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
        txn.commit().map_err(mace_error)?;
        let level = capacity_level(next.bytes, available, self.budget.limit);
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
                    let batch = pb::TelemetryBatch::decode(
                        zstd::decode_all(raw.slice())?.as_slice(),
                    )?;
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
            "remainingBytes": available,
            "capacityLevel": capacity_level(meta.bytes, available, self.budget.limit),
            "oldestAt": if pending > 0 { meta.oldest_at } else { 0 },
            "exhaustEtaSecs": if rate > 0 { available / rate.max(1) } else { 0 },
            "lastFailureAt": LAST_FAILURE.load(Ordering::Relaxed),
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

pub fn record_gap(kind: &str, at: i64, reason: &str) {
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
    // Emit one durable gap marker per incident start; re-emits only after the
    // marker itself was acknowledged upstream (gap start resets on resolution).
    let start = GAP_START.load(Ordering::Relaxed);
    if GAP_REPORTED.fetch_max(start, Ordering::Relaxed) < start {
        emit_meta("gap", kind, reason, start, at);
    }
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
    let path = NodePaths::current().data_dir().join("telemetry");
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
pub async fn enqueue_prepared(batch: pb::TelemetryBatch) -> anyhow::Result<()> {
    let spool = queue(&batch.kind)?;
    tokio::task::spawn_blocking(move || spool.append_prepared(batch)).await??;
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
        Some(q) => serde_json::json!({
            "core": q.core.snapshot(),
            "detail": q.detail.snapshot(),
            "gapStart": GAP_START.load(Ordering::Relaxed),
            "gapEnd": GAP_END.load(Ordering::Relaxed),
            "gapHasCore": GAP_CORE.load(Ordering::Relaxed) != 0,
        }),
        None => serde_json::json!({"status": "unavailable"}),
    }
}
type Upload = (
    String,
    u64,
    tokio::task::JoinHandle<anyhow::Result<pb::TelemetryReceipt>>,
);

async fn settle_uploads(spool: Arc<Spool>, uploads: Vec<Upload>) -> (usize, Option<anyhow::Error>) {
    let mut confirmed = 0;
    let mut failure = None;
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
                warn!(%batch_id, sequence, error=%err, "telemetry upload remains unconfirmed");
                if failure.is_none() {
                    failure = Some(err);
                }
            }
        }
    }
    (confirmed, failure)
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
        let (prefetched, next_cursor) = match tokio::task::spawn_blocking(move || {
            read.replay_window(cursor)
        })
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
                    tokio::time::timeout(Duration::from_secs(30), service.upload_batch(batch)).await;
                match receipt {
                    Ok(Ok(receipt)) => Ok(receipt.into_inner()),
                    Ok(Err(err)) => Err(anyhow::Error::from(err)),
                    Err(err) => Err(anyhow::Error::from(err)),
                }
            });
            uploads.push((batch_id, sequence, handle));
        }
        let (confirmed, failure) = settle_uploads(spool.clone(), uploads).await;
        cursor = next_cursor;
        if let Some(err) = failure {
            // A poison head must not impose minute-long retries on healthy
            // observations. Errors remain visible and their payloads retained.
            if confirmed > 0 {
                LAST_FAILURE.store(crate::utils::time::now_timestamp(), Ordering::Relaxed);
                warn!(core, confirmed, error=%err, "telemetry replay partially confirmed; rejected batches retained");
                failures = failures.saturating_add(1);
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                backoff(core, &mut failures, err).await;
            }
        } else {
            failures = 0;
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
    let request = pb::CreateHttpAccessLogsRequest {
        http_access_logs: std::mem::take(buffer),
    };
    if let Err(e) = enqueue("access_log", &request, 0, false).await {
        record_gap(
            "access_log",
            crate::utils::time::now_timestamp(),
            &e.to_string(),
        );
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
    let request = pb::CreateNodeLogsRequest {
        node_logs: std::mem::take(buffer),
    };
    if let Err(e) = enqueue("node_log", &request, 0, false).await {
        record_gap(
            "node_log",
            crate::utils::time::now_timestamp(),
            &e.to_string(),
        );
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
        declared
            .min(available.saturating_sub(available_bytes(total, available, 0)))
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
            spool.append("probe_observation", vec![at as u8], 0, false, at).unwrap();
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
        spool.append("probe_observation", vec![2], 0, false, 200).unwrap();
        let first = spool.at(0).unwrap().unwrap();
        let second = spool.at(1).unwrap().unwrap();
        let good = receipt_for(&second);
        let uploads = vec![
            (first.batch_id.clone(), first.sequence, tokio::spawn(async {
                Err(anyhow::Error::from(tonic::Status::unavailable("assignment unavailable")))
            })),
            (second.batch_id, second.sequence, tokio::spawn(async { Ok(good) })),
        ];
        let (confirmed, failure) = settle_uploads(spool.clone(), uploads).await;
        assert_eq!(confirmed, 1);
        assert!(failure.unwrap().to_string().contains("assignment unavailable"));
        assert_eq!(spool.front().unwrap().unwrap().batch_id, first.batch_id);
        assert_eq!(spool.snapshot()["pendingBatches"], 1);
        drop(spool);
        std::fs::remove_dir_all(path).unwrap();
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
        assert_eq!(
            available_bytes(100 * GIB, 90 * GIB, 0),
            90 * GIB - 2 * GIB
        );
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
        while core.append("node_metric", vec![7; 64 * 1024], 0, false, 1).is_ok() {
            appended += 1;
        }
        assert!(appended > 0);
        // The queue stops just short of its ceiling rather than overrunning it.
        assert!(core.budget.counter.used() > 0);
        assert!(core.budget.counter.used() <= MIB);
        // Core is done for this window; it cannot spend detail's slice.
        assert!(core.append("node_metric", vec![7; 64 * 1024], 0, false, 1).is_err());

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
                for (lane, capacity, is_core) in
                    [("core", core, true), ("detail", detail, false)]
                {
                    assert!(capacity <= available, "{lane} over-quota: {total}/{available}");
                    let released = available - capacity;
                    let base = (total / RESERVE_RATIO).min(
                        (available / RESERVE_AVAILABLE_DIVISOR).max(RESERVE_FLOOR),
                    );
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
    // identity and hands it to telemetry::enqueue for "probe_observation".
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
