//! Durable producer queues. API memory is not a delivery acknowledgement.
use crate::{api_config::ApiConfig, pb, paths::NodePaths, rpc::client::SharedRpcClient};
use mace::{Bucket, BucketOptions, Mace, OpCode, Options};
use parking_lot::Mutex;
use prost::Message;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicI64, AtomicU8, Ordering},
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
        }
    }
}
pub struct Spool {
    _db: Mace,
    bucket: Bucket,
    meta: Mutex<Meta>,
    path: PathBuf,
    core: bool,
    limit: u64,
}
fn mace_error(e: OpCode) -> anyhow::Error {
    anyhow::anyhow!("telemetry spool: {e:?}")
}
impl Spool {
    pub fn open(path: &Path, core: bool, limit: u64) -> anyhow::Result<Self> {
        std::fs::create_dir_all(path)?;
        let mut opts = Options::new(path);
        opts.sync_on_write = true;
        opts.wal_buffer_size = 1 << 20;
        opts.wal_file_size = 4 << 20;
        opts.lru_capacity = 8 << 20;
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
        Ok(Self {
            _db: db,
            bucket,
            meta: Mutex::new(meta),
            path: path.to_owned(),
            core,
            limit,
        })
    }
    fn capacity(&self) -> u64 {
        if self.limit > 0 {
            return self.limit;
        }
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let disk = disks
            .iter()
            .filter(|d| self.path.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len());
        match disk {
            Some(d) => {
                let reserve = (d.total_space() / 50).max(1 << 30);
                let core_reserve = if self.core { 0 } else { 256 << 20 };
                d.available_space().saturating_sub(reserve + core_reserve)
            }
            None => 0,
        }
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
        if encoded.len() as u64 > available
            || self.limit > 0 && meta.bytes.saturating_add(encoded.len() as u64) > available
        {
            // The caller decides whether the batch is dropped (record a gap)
            // or retained in memory for the next window — a full spool is not
            // itself proof of loss.
            anyhow::bail!("telemetry spool exhausted")
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
        let level = capacity_level(next.bytes, available, self.limit);
        *meta = next;
        escalate_capacity(level, self.core);
        Ok(batch.batch_id)
    }
    pub fn front(&self) -> anyhow::Result<Option<pb::TelemetryBatch>> {
        let meta = self.meta.lock();
        if meta.head == meta.tail {
            return Ok(None);
        }
        let view = self.bucket.view().map_err(mace_error)?;
        let raw = view
            .get(format!("batch:{:020}", meta.head).as_bytes())
            .map_err(mace_error)?;
        let decoded = zstd::decode_all(raw.slice())?;
        Ok(Some(pb::TelemetryBatch::decode(decoded.as_slice())?))
    }
    pub fn acknowledge(&self, receipt: &pb::TelemetryReceipt) -> anyhow::Result<()> {
        let mut meta = self.meta.lock();
        if receipt.sequence != meta.head
            || receipt.batch_id != format!("{}:{}", meta.boot, meta.head)
            || receipt.durable_at <= 0
            || receipt.backend != "direct"
        {
            anyhow::bail!("invalid durable telemetry receipt")
        }
        let key = format!("batch:{:020}", meta.head);
        let txn = self.bucket.begin().map_err(mace_error)?;
        let size = txn.get(key.as_bytes()).map_err(mace_error)?.slice().len();
        let mut next = meta.clone();
        next.head += 1;
        next.bytes = next.bytes.saturating_sub(size as u64);
        if next.head == next.tail {
            next.oldest_at = 0;
        } else {
            let raw = txn
                .get(format!("batch:{:020}", next.head).as_bytes())
                .map_err(mace_error)?;
            let decoded = zstd::decode_all(raw.slice())?;
            next.oldest_at = pb::TelemetryBatch::decode(decoded.as_slice())?.occurred_at;
        }
        txn.del(key.as_bytes()).map_err(mace_error)?;
        txn.upsert(b"meta", serde_json::to_vec(&next)?)
            .map_err(mace_error)?;
        txn.commit().map_err(mace_error)?;
        let level = capacity_level(next.bytes, self.capacity(), self.limit);
        *meta = next;
        // Falling below a threshold re-arms escalation for the next incident.
        ALERT_LEVEL.fetch_min(level, Ordering::Relaxed);
        Ok(())
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
        let pending = meta.tail - meta.head;
        let elapsed = crate::utils::time::now_timestamp() - meta.meter_started;
        let rate = if pending > 0 && elapsed > 0 {
            meta.meter_bytes / elapsed as u64
        } else {
            0
        };
        serde_json::json!({
            "pendingBatches": pending,
            "pendingBytes": meta.bytes,
            "remainingBytes": available,
            "capacityLevel": capacity_level(meta.bytes, available, self.limit),
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
    error!(kind, reason, at, "TELEMETRY DATA GAP: service continues; affected billing windows must remain unsettled");
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
            &format!("persistent {} buffer at {}% of budget", if core { "core" } else { "detail" }, level),
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
    let detail = Arc::new(Spool::open(&path.join("detail"), false, 0)?);
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
    QUEUES
        .get()
        .map(|q| q.core.boot_id())
        .unwrap_or_default()
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
    loop {
        let read = spool.clone();
        let batch = match tokio::task::spawn_blocking(move || read.front()).await {
            Ok(Ok(Some(b))) => b,
            Ok(Ok(None)) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            result => {
                error!(?result, "telemetry persistent queue read failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let result = async {
            let client = SharedRpcClient::get(&api).await?.as_rpc_client();
            let mut service = client.telemetry_service();
            let receipt = tokio::time::timeout(
                Duration::from_secs(30),
                service.upload_batch(batch),
            )
            .await??
            .into_inner();
            let ack = spool.clone();
            tokio::task::spawn_blocking(move || ack.acknowledge(&receipt)).await??;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                failures = 0;
            }
            Err(e) => {
                if failures == 0 {
                    LAST_FAILURE
                        .store(crate::utils::time::now_timestamp(), Ordering::Relaxed);
                    error!(core, error=%e, "URGENT: telemetry write failed; retained on node until durable receipt");
                } else {
                    warn!(core, error=%e, "telemetry batch remains pending");
                }
                failures = failures.saturating_add(1);
                tokio::time::sleep(Duration::from_secs(
                    (1u64 << failures.min(6)).min(60),
                ))
                .await;
            }
        }
    }
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
        record_gap("access_log", crate::utils::time::now_timestamp(), &e.to_string());
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
        record_gap("node_log", crate::utils::time::now_timestamp(), &e.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(spool
            .acknowledge(&pb::TelemetryReceipt {
                batch_id: "wrong".into(),
                sequence: b.sequence,
                backend: "direct".into(),
                durable_at: 2,
            })
            .is_err());
        assert!(spool.front().unwrap().is_some());
        spool.acknowledge(&pb::TelemetryReceipt {
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
            if spool.append("node_metric", vec![1; 100], 0, false, 2).is_err() {
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
