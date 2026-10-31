//! Issue 34 full-Edge startup controller fixture (test-only).
//!
//! The fixture lets the real `Zelvik-Edge` binary execute its production
//! startup path: ControlChannel activation, an HMAC-authenticated NodeControl
//! snapshot, then data-plane listener binding. It never stands in for the
//! component lane and it never claims a listener is healthy without observing
//! it.
//!
//! Telemetry has three fixture modes. `unavailable` is the long-lane default:
//! a real `pb.TelemetryService` `uploadBatch` keeps returning gRPC
//! `UNAVAILABLE`, so the Edge replay path retains the batches and retries with
//! its own bounded backoff. `unimplemented` preserves the smoke assertion that
//! a terminal rejection is quarantined with a durable gap. `success` appends
//! the complete batch to a durable fixture store and fsyncs before returning a
//! receipt; it never fakes an acknowledgement. Modes can be switched at
//! runtime through `--telemetry-mode-file` or the local `POST /telemetry-mode`
//! admin endpoint while the same Edge process stays alive.
//!
//! `tests/issue34_full_edge.rs` includes this file via `#[path]` and passes
//! `CARGO_BIN_EXE_Zelvik-Edge`; `scripts/memory/full_edge.py` runs the example
//! binary in `serve` mode so `scripts/memory/run_soak.py` can drive the real
//! Edge unchanged.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{TcpListener as StdTcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail};
use hmac::{Hmac, KeyInit, Mac};
use prost::Message;
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use zelvik_edge::node_control::proto::node_control_server::{
    NodeControl, NodeControlServer,
};
use zelvik_edge::node_control::proto::{
    ApplyReleaseRequest, ApplyReleaseResponse, EnvelopeMeta, GetDesiredReleaseRequest,
    GetDesiredReleaseResponse, RegisterNodeRequest, RegisterNodeResponse,
    ReportNodeHealthRequest, ReportNodeHealthResponse, StreamEventsRequest,
    StreamEventsResponse,
};
use zelvik_suite::pb::control::v1::control_channel_server::{
    ControlChannel, ControlChannelServer,
};
use zelvik_edge::pb::{TelemetryBatch, TelemetryReceipt};
use zelvik_suite::pb::control::v1::{
    HeartbeatRequest, HeartbeatResponse, RegisterInstanceRequest, RegisterInstanceResponse,
    RotateCredentialRequest, RotateCredentialResponse,
};

pub const TEST_SECRET: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
pub const TEST_INSTANCE: &str = "nd_0123456789";
pub const PAYLOAD_PATH: &str = "/issue34/payload";
const MAX_EDGE_LOG_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEALTH_FIELD_BYTES: usize = 1024 * 1024;
const MAX_ERRORS: usize = 32;
const MAX_ERROR_CHARS: usize = 512;

#[derive(Clone, Debug)]
pub struct Options {
    pub home: PathBuf,
    pub edge_binary: Option<PathBuf>,
    /// Foreground subcommand of the binary under test. Clap derives this from
    /// the `_StartInternal` variant as `start-internal`; the old literal
    /// `_StartInternal` is rejected with exit 2. Keep it configurable so a
    /// future rename is a one-line fixture change, not a hidden mismatch.
    pub edge_arg: String,
    pub grpc_listen: String,
    pub status_listen: String,
    /// 0 asks the fixture to reserve a free loopback port.
    pub tls_port: u16,
    pub tls_host: String,
    pub origin: String,
    pub cert_dir: Option<PathBuf>,
    pub ca_file: Option<PathBuf>,
    pub cert_file: Option<PathBuf>,
    pub key_file: Option<PathBuf>,
    pub secret: String,
    pub instance_id: String,
    pub version: i64,
    pub status_file: Option<PathBuf>,
    pub ready_file: Option<PathBuf>,
    pub report_file: Option<PathBuf>,
    pub edge_log: Option<PathBuf>,
    /// `unavailable` (long lane, retryable), `unimplemented` (terminal smoke
    /// rejection), or `success` (durable store + real receipt).
    pub telemetry_mode: String,
    pub telemetry_mode_file: Option<PathBuf>,
    pub telemetry_store: Option<PathBuf>,
    pub telemetry_store_max_bytes: u64,
    pub spawn_edge: bool,
    pub smoke: bool,
    pub require_health: bool,
    pub cleanup_home: bool,
}

impl Options {
    pub fn fixture(home: impl Into<PathBuf>, edge_binary: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            edge_binary: Some(edge_binary.into()),
            edge_arg: "start-internal".into(),
            grpc_listen: "127.0.0.1:0".into(),
            status_listen: "127.0.0.1:0".into(),
            tls_port: 0,
            tls_host: "localhost".into(),
            origin: "auto".into(),
            cert_dir: None,
            ca_file: None,
            cert_file: None,
            key_file: None,
            secret: TEST_SECRET.into(),
            instance_id: TEST_INSTANCE.into(),
            version: 1,
            status_file: None,
            ready_file: None,
            report_file: None,
            edge_log: None,
            telemetry_mode: "unavailable".into(),
            telemetry_mode_file: None,
            telemetry_store: None,
            telemetry_store_max_bytes: 256 * 1024 * 1024,
            spawn_edge: true,
            smoke: true,
            require_health: true,
            cleanup_home: false,
        }
    }

    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> anyhow::Result<Self> {
        let mut opts = Self::fixture(".", "Zelvik-Edge");
        opts.spawn_edge = false;
        opts.smoke = false;
        opts.home = PathBuf::new();
        opts.edge_binary = None;
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
                None => (arg, None),
            };
            let mut value = |name: &str| -> anyhow::Result<String> {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .with_context(|| format!("{name} requires a value"))
            };
            match flag.as_str() {
                "--home" => opts.home = PathBuf::from(value("--home")?),
                "--edge-binary" => {
                    opts.edge_binary = Some(PathBuf::from(value("--edge-binary")?))
                }
                "--edge-arg" => opts.edge_arg = value("--edge-arg")?,
                "--grpc-listen" => opts.grpc_listen = value("--grpc-listen")?,
                "--status-listen" => opts.status_listen = value("--status-listen")?,
                "--tls-port" => opts.tls_port = value("--tls-port")?.parse()?,
                "--tls-host" => opts.tls_host = value("--tls-host")?,
                "--origin" => opts.origin = value("--origin")?,
                "--cert-dir" => opts.cert_dir = Some(PathBuf::from(value("--cert-dir")?)),
                "--ca-file" => opts.ca_file = Some(PathBuf::from(value("--ca-file")?)),
                "--cert-file" => opts.cert_file = Some(PathBuf::from(value("--cert-file")?)),
                "--key-file" => opts.key_file = Some(PathBuf::from(value("--key-file")?)),
                "--secret" => opts.secret = value("--secret")?,
                "--instance-id" => opts.instance_id = value("--instance-id")?,
                "--version" => opts.version = value("--version")?.parse()?,
                "--status-file" => opts.status_file = Some(PathBuf::from(value("--status-file")?)),
                "--ready-file" => opts.ready_file = Some(PathBuf::from(value("--ready-file")?)),
                "--report-file" => {
                    opts.report_file = Some(PathBuf::from(value("--report-file")?))
                }
                "--edge-log" => opts.edge_log = Some(PathBuf::from(value("--edge-log")?)),
                "--telemetry-mode" => opts.telemetry_mode = value("--telemetry-mode")?,
                "--telemetry-mode-file" => {
                    opts.telemetry_mode_file = Some(PathBuf::from(value("--telemetry-mode-file")?))
                }
                "--telemetry-store" => {
                    opts.telemetry_store = Some(PathBuf::from(value("--telemetry-store")?))
                }
                "--telemetry-store-max-bytes" => {
                    opts.telemetry_store_max_bytes = value("--telemetry-store-max-bytes")?.parse()?
                }
                "--spawn-edge" => opts.spawn_edge = true,
                "--smoke" => opts.smoke = true,
                "--no-require-health" => opts.require_health = false,
                "--cleanup-home" => opts.cleanup_home = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown argument {other:?}"),
            }
        }
        if opts.home.as_os_str().is_empty() {
            bail!("--home is required");
        }
        if opts.spawn_edge && opts.edge_binary.is_none() {
            bail!("--spawn-edge requires --edge-binary");
        }
        if opts.smoke && !opts.spawn_edge {
            bail!("--smoke requires --spawn-edge");
        }
        // Parse once so a typo fails before an Edge process is spawned.
        let mode = TelemetryMode::parse(&opts.telemetry_mode)?;
        if mode == TelemetryMode::Success && opts.telemetry_store.is_none() {
            bail!("--telemetry-mode success requires --telemetry-store");
        }
        Ok(opts)
    }
}

fn print_help() {
    println!(
        "issue34-controller [--home DIR] [--edge-binary PATH] [--spawn-edge] [--smoke]\n\
         [--grpc-listen ADDR] [--status-listen ADDR] [--tls-port PORT] [--tls-host HOST]\n\
         [--origin auto|http://HOST:PORT] [--cert-dir DIR] [--status-file FILE]\n\
         [--ready-file FILE] [--report-file FILE] [--edge-log FILE]\n\
         [--telemetry-mode unavailable|unimplemented|success]\n\
         [--telemetry-mode-file FILE] [--telemetry-store FILE]\n\
         [--telemetry-store-max-bytes BYTES]"
    );
}

// ---------------------------------------------------------------------------
// Telemetry fixture backend

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TelemetryMode {
    /// Retryable transport fault: the Edge must retain the batch and retry.
    Unavailable,
    /// Terminal rejection: the Edge quarantines the head and records a gap.
    Unimplemented,
    /// Durable fixture store: fsync the complete batch before the receipt.
    Success,
}

impl TelemetryMode {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim() {
            "unavailable" => Ok(Self::Unavailable),
            "unimplemented" => Ok(Self::Unimplemented),
            "success" => Ok(Self::Success),
            other => bail!(
                "unknown telemetry mode {other:?}; expected unavailable, unimplemented, or success"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Unimplemented => "unimplemented",
            Self::Success => "success",
        }
    }

    fn expected_code(self) -> (&'static str, u32) {
        match self {
            Self::Unavailable => ("UNAVAILABLE", 14),
            Self::Unimplemented => ("UNIMPLEMENTED", 12),
            Self::Success => ("OK", 0),
        }
    }

    fn classification(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "retryable; complete batches retained and retried with bounded backoff"
            }
            Self::Unimplemented => "terminal rejection; head quarantined with a durable gap",
            Self::Success => "durably stored complete batch; receipt returned after fsync",
        }
    }
}

/// Append-only durable backend for `success` mode.
///
/// Each record is `u32` big-endian length + the complete encoded
/// `TelemetryBatch`, and `sync_all` happens before the receipt leaves the
/// process. A failed write returns `UNAVAILABLE`, never a fake receipt.
struct TelemetryStore {
    path: PathBuf,
    max_bytes: u64,
    file: Mutex<Option<File>>,
    bytes: AtomicU64,
    writes: AtomicU64,
}

impl TelemetryStore {
    fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            file: Mutex::new(None),
            bytes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }

    fn append(&self, batch: &TelemetryBatch) -> anyhow::Result<u64> {
        let encoded = batch.encode_to_vec();
        let mut file = self.file.lock().unwrap();
        if file.is_none() {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            *file = Some(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                    .with_context(|| format!("open telemetry store {}", self.path.display()))?,
            );
        }
        let file = file.as_mut().context("telemetry store is not open")?;
        let record = 4u64
            .checked_add(encoded.len() as u64)
            .context("telemetry record length overflow")?;
        let current = file.metadata()?.len();
        let next = current
            .checked_add(record)
            .context("telemetry store size overflow")?;
        anyhow::ensure!(
            next <= self.max_bytes,
            "telemetry store is full: {next} bytes would exceed {}",
            self.max_bytes
        );
        file.write_all(&(encoded.len() as u32).to_be_bytes())?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        self.bytes.store(next, Ordering::Relaxed);
        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(record)
    }

    fn json(&self) -> Value {
        json!({
            "enabled": true,
            "path": self.path,
            "bytes": self.bytes.load(Ordering::Relaxed),
            "writes": self.writes.load(Ordering::Relaxed),
            "maxBytes": self.max_bytes,
        })
    }
}

fn is_core_telemetry_kind(kind: &str) -> bool {
    matches!(kind, "bandwidth" | "node_metric" | "gap" | "alert" | "daily" | "unique_ip")
}

#[derive(Default)]
struct TelemetryCounters {
    requests: AtomicU64,
    unavailable: AtomicU64,
    unimplemented: AtomicU64,
    success: AtomicU64,
    other: AtomicU64,
    store_errors: AtomicU64,
    mode_switches: AtomicU64,
    core_max_sequence: AtomicU64,
    core_acked_sequence: AtomicU64,
    detail_max_sequence: AtomicU64,
    detail_acked_sequence: AtomicU64,
    max_pending: AtomicU64,
    max_batch_bytes: AtomicU64,
    total_batch_bytes: AtomicU64,
    inflight: AtomicI64,
    max_inflight: AtomicI64,
}

impl TelemetryCounters {
    fn observe(&self, batch: &TelemetryBatch) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if is_core_telemetry_kind(&batch.kind) {
            self.core_max_sequence
                .fetch_max(batch.sequence, Ordering::Relaxed);
        } else {
            self.detail_max_sequence
                .fetch_max(batch.sequence, Ordering::Relaxed);
        }
        self.max_batch_bytes.fetch_max(
            batch.encoded_len() as u64,
            Ordering::Relaxed,
        );
        self.total_batch_bytes
            .fetch_add(batch.encoded_len() as u64, Ordering::Relaxed);
        self.max_pending.fetch_max(self.pending(), Ordering::Relaxed);
    }

    fn ack(&self, batch: &TelemetryBatch) {
        if is_core_telemetry_kind(&batch.kind) {
            self.core_acked_sequence
                .fetch_max(batch.sequence, Ordering::Relaxed);
        } else {
            self.detail_acked_sequence
                .fetch_max(batch.sequence, Ordering::Relaxed);
        }
    }

    /// Core and detail are independent sequence spaces; pending is their sum,
    /// never one lane's max minus another lane's ack.
    fn pending(&self) -> u64 {
        let core = self
            .core_max_sequence
            .load(Ordering::Relaxed)
            .saturating_sub(self.core_acked_sequence.load(Ordering::Relaxed));
        let detail = self
            .detail_max_sequence
            .load(Ordering::Relaxed)
            .saturating_sub(self.detail_acked_sequence.load(Ordering::Relaxed));
        core.saturating_add(detail)
    }

    fn begin_inflight(&self) {
        let inflight = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_inflight.fetch_max(inflight, Ordering::Relaxed);
    }

    fn end_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    fn json(&self) -> Value {
        let core_max = self.core_max_sequence.load(Ordering::Relaxed);
        let core_acked = self.core_acked_sequence.load(Ordering::Relaxed);
        let detail_max = self.detail_max_sequence.load(Ordering::Relaxed);
        let detail_acked = self.detail_acked_sequence.load(Ordering::Relaxed);
        json!({
            "requests": self.requests.load(Ordering::Relaxed),
            "unavailable": self.unavailable.load(Ordering::Relaxed),
            "unimplemented": self.unimplemented.load(Ordering::Relaxed),
            "success": self.success.load(Ordering::Relaxed),
            "other": self.other.load(Ordering::Relaxed),
            "storeErrors": self.store_errors.load(Ordering::Relaxed),
            "modeSwitches": self.mode_switches.load(Ordering::Relaxed),
            "maxSequence": core_max.max(detail_max),
            "ackedSequence": core_acked.max(detail_acked),
            "coreMaxSequence": core_max,
            "coreAckedSequence": core_acked,
            "corePendingBatches": core_max.saturating_sub(core_acked),
            "detailMaxSequence": detail_max,
            "detailAckedSequence": detail_acked,
            "detailPendingBatches": detail_max.saturating_sub(detail_acked),
            // Scalars only: never retain raw batches or per-sequence arrays.
            "pendingBatches": core_max.saturating_sub(core_acked)
                + detail_max.saturating_sub(detail_acked),
            "maxPendingBatches": self.max_pending.load(Ordering::Relaxed),
            "maxBatchBytes": self.max_batch_bytes.load(Ordering::Relaxed),
            "totalBatchBytes": self.total_batch_bytes.load(Ordering::Relaxed),
            "inflight": self.inflight.load(Ordering::Relaxed),
            "maxInflight": self.max_inflight.load(Ordering::Relaxed),
        })
    }
}

// ---------------------------------------------------------------------------
// Controller state

struct Controller {
    home: PathBuf,
    pid_file: PathBuf,
    secret: String,
    version: i64,
    snapshot: Vec<u8>,
    tls: Value,
    status_file: Option<PathBuf>,
    counters: Counters,
    telemetry_mode: Mutex<TelemetryMode>,
    telemetry_mode_file: Option<PathBuf>,
    telemetry_store: Option<Arc<TelemetryStore>>,
    telemetry_counters: TelemetryCounters,
    health: Mutex<Option<Value>>,
    errors: Mutex<VecDeque<String>>,
}

#[derive(Default)]
struct Counters {
    register_node: AtomicU64,
    get_desired_release: AtomicU64,
    apply_release: AtomicU64,
    stream_events: AtomicU64,
    report_health: AtomicU64,
    register_instance: AtomicU64,
    heartbeat: AtomicU64,
}

impl Counters {
    fn json(&self) -> Value {
        json!({
            "registerNode": self.register_node.load(Ordering::Relaxed),
            "getDesiredRelease": self.get_desired_release.load(Ordering::Relaxed),
            "applyRelease": self.apply_release.load(Ordering::Relaxed),
            "streamEvents": self.stream_events.load(Ordering::Relaxed),
            "reportNodeHealth": self.report_health.load(Ordering::Relaxed),
            "registerInstance": self.register_instance.load(Ordering::Relaxed),
            "heartbeat": self.heartbeat.load(Ordering::Relaxed),
        })
    }
}

impl Controller {
    /// File mode wins over the in-memory value, so a soak driver can switch
    /// the backend without restarting the controller or the Edge process.
    fn effective_telemetry_mode(&self) -> Result<TelemetryMode, Status> {
        if let Some(path) = &self.telemetry_mode_file
            && let Ok(raw) = fs::read_to_string(path)
            && !raw.trim().is_empty()
        {
            let mode = TelemetryMode::parse(&raw)
                .map_err(|err| Status::failed_precondition(err.to_string()))?;
            *self.telemetry_mode.lock().unwrap() = mode;
            return Ok(mode);
        }
        Ok(*self.telemetry_mode.lock().unwrap())
    }

    fn set_telemetry_mode(&self, mode: TelemetryMode) -> anyhow::Result<()> {
        if mode == TelemetryMode::Success && self.telemetry_store.is_none() {
            bail!("telemetry mode success requires --telemetry-store");
        }
        if let Some(path) = &self.telemetry_mode_file {
            atomic_write(path, mode.as_str().as_bytes())?;
        }
        *self.telemetry_mode.lock().unwrap() = mode;
        self.telemetry_counters
            .mode_switches
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn current_telemetry_mode(&self) -> TelemetryMode {
        self.effective_telemetry_mode()
            .unwrap_or_else(|_| *self.telemetry_mode.lock().unwrap())
    }

    fn telemetry_contract(&self) -> Value {
        let mode = self.current_telemetry_mode();
        let (code, number) = mode.expected_code();
        json!({
            "served": mode == TelemetryMode::Success,
            "mode": mode.as_str(),
            "modes": ["unavailable", "unimplemented", "success"],
            "service": "pb.TelemetryService",
            "method": "uploadBatch",
            "expected_grpc_code": code,
            "expected_code_number": number,
            "edge_classification": mode.classification(),
            "why": "success mode fsyncs the complete batch before acknowledging; the other modes never ack",
            "modeFile": self.telemetry_mode_file,
            "store": self.telemetry_store.as_ref().map(|store| store.json()),
            "counters": self.telemetry_counters.json(),
        })
    }

    fn record_error(&self, message: impl Into<String>) {
        let mut message = message.into();
        message.truncate(MAX_ERROR_CHARS);
        let mut errors = self.errors.lock().unwrap();
        if errors.len() >= MAX_ERRORS {
            errors.pop_front();
        }
        errors.push_back(message);
    }

    fn health_value(&self) -> Option<Value> {
        self.health.lock().unwrap().clone()
    }

    fn status_doc(&self) -> Value {
        let health = self.health_value();
        let governor = health
            .as_ref()
            .and_then(|value| value.get("resourceGovernor"))
            .cloned()
            .unwrap_or(Value::Null);
        let errors: Vec<String> = self.errors.lock().unwrap().iter().cloned().collect();
        json!({
            "schema": "issue34-full-edge-controller/v1",
            "ready": health.is_some(),
            "home": self.home,
            "pidFile": self.pid_file,
            "tls": self.tls,
            "counters": self.counters.json(),
            "telemetry": self.telemetry_contract(),
            "resourceGovernor": governor,
            "health": health,
            "errors": errors,
            "updatedAt": now_secs(),
        })
    }

    fn write_status(&self) {
        let doc = self.status_doc();
        if let Some(path) = &self.status_file
            && let Err(err) = atomic_write_json(path, &doc)
        {
            self.record_error(format!("status file {}: {err}", path.display()));
        }
    }

    fn install_health(&self, request: &ReportNodeHealthRequest) {
        let mut parsed = serde_json::Map::new();
        for (field, raw) in [
            ("resourceGovernor", request.resource_pressure_json.as_str()),
            ("listeners", request.listeners_json.as_str()),
            ("cache", request.cache_json.as_str()),
            ("xdp", request.xdp_json.as_str()),
            ("taskStatus", request.task_status_json.as_str()),
        ] {
            if raw.len() > MAX_HEALTH_FIELD_BYTES {
                self.record_error(format!(
                    "health field {field} is {} bytes, over the {MAX_HEALTH_FIELD_BYTES} byte bound",
                    raw.len()
                ));
                parsed.insert(field.into(), Value::Null);
                continue;
            }
            match serde_json::from_str::<Value>(raw) {
                Ok(value) => {
                    parsed.insert(field.into(), value);
                }
                Err(err) => {
                    self.record_error(format!("health field {field} is not JSON: {err}"));
                    parsed.insert(field.into(), Value::Null);
                }
            }
        }
        parsed.insert("ready".into(), json!(request.ready));
        parsed.insert("receivedAt".into(), json!(now_secs()));
        let value = Value::Object(parsed);
        *self.health.lock().unwrap() = Some(value);
        self.write_status();
    }
}

#[tonic::async_trait]
impl NodeControl for Controller {
    async fn register_node(
        &self,
        request: Request<RegisterNodeRequest>,
    ) -> Result<Response<RegisterNodeResponse>, Status> {
        self.counters.register_node.fetch_add(1, Ordering::Relaxed);
        let node = request.into_inner();
        if node.node_id.trim().is_empty() {
            self.record_error("RegisterNode carried an empty node_id");
            return Err(Status::invalid_argument("node_id is required"));
        }
        if node.node_id != self.instance_id() {
            self.record_error(format!(
                "RegisterNode node_id {:?} does not match the fixture instance",
                node.node_id
            ));
            return Err(Status::permission_denied("node identity mismatch"));
        }
        Ok(Response::new(RegisterNodeResponse {
            meta: Some(envelope(self.version)),
            accepted: true,
            enrollment_status: "active".into(),
        }))
    }

    async fn get_desired_release(
        &self,
        request: Request<GetDesiredReleaseRequest>,
    ) -> Result<Response<GetDesiredReleaseResponse>, Status> {
        self.counters
            .get_desired_release
            .fetch_add(1, Ordering::Relaxed);
        let requested = request.into_inner();
        if requested.node_id.trim().is_empty() {
            self.record_error("GetDesiredRelease carried an empty node_id");
            return Err(Status::invalid_argument("node_id is required"));
        }
        if requested.node_id != self.instance_id() {
            self.record_error(format!(
                "GetDesiredRelease node_id {:?} does not match the fixture instance",
                requested.node_id
            ));
            return Err(Status::permission_denied("node identity mismatch"));
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_bytes())
            .map_err(|_| Status::internal("fixture HMAC key rejected"))?;
        mac.update(&self.version.to_be_bytes());
        mac.update(&self.snapshot);
        Ok(Response::new(GetDesiredReleaseResponse {
            meta: Some(envelope(self.version)),
            release_id: String::new(),
            signature_digest: Vec::new(),
            snapshot_required: true,
            snapshot_json: self.snapshot.clone(),
            snapshot_hmac: mac.finalize().into_bytes().to_vec(),
        }))
    }

    async fn apply_release(
        &self,
        request: Request<ApplyReleaseRequest>,
    ) -> Result<Response<ApplyReleaseResponse>, Status> {
        self.counters.apply_release.fetch_add(1, Ordering::Relaxed);
        let request = request.into_inner();
        if !matches!(
            request.phase.as_str(),
            "prepare" | "validate" | "apply" | "ack"
        ) {
            return Err(Status::invalid_argument("unknown ApplyRelease phase"));
        }
        Ok(Response::new(ApplyReleaseResponse {
            meta: request.meta,
            idempotent_replay: false,
            phase_status: request.phase,
        }))
    }

    type StreamEventsStream =
        tokio_stream::Empty<Result<StreamEventsResponse, Status>>;

    async fn stream_events(
        &self,
        _request: Request<tonic::Streaming<StreamEventsRequest>>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        self.counters.stream_events.fetch_add(1, Ordering::Relaxed);
        Ok(Response::new(tokio_stream::empty()))
    }

    async fn report_node_health(
        &self,
        request: Request<ReportNodeHealthRequest>,
    ) -> Result<Response<ReportNodeHealthResponse>, Status> {
        self.counters.report_health.fetch_add(1, Ordering::Relaxed);
        let request = request.into_inner();
        if request.node_id.trim().is_empty() {
            self.record_error("ReportNodeHealth carried an empty node_id");
            return Err(Status::invalid_argument("node_id is required"));
        }
        if request.node_id != self.instance_id() {
            self.record_error(format!(
                "ReportNodeHealth node_id {:?} does not match the fixture instance",
                request.node_id
            ));
            return Err(Status::permission_denied("node identity mismatch"));
        }
        self.install_health(&request);
        Ok(Response::new(ReportNodeHealthResponse {
            meta: Some(envelope(self.version)),
            accepted: true,
        }))
    }
}

#[tonic::async_trait]
impl ControlChannel for Controller {
    async fn register_instance(
        &self,
        request: Request<RegisterInstanceRequest>,
    ) -> Result<Response<RegisterInstanceResponse>, Status> {
        self.counters
            .register_instance
            .fetch_add(1, Ordering::Relaxed);
        let request = request.into_inner();
        if request.instance_id != self.instance_id() {
            self.record_error(format!(
                "RegisterInstance instance_id {:?} does not match the fixture",
                request.instance_id
            ));
            return Err(Status::permission_denied("instance id mismatch"));
        }
        Ok(Response::new(RegisterInstanceResponse {
            accepted: true,
            enrollment_status: "active".into(),
            heartbeat_interval_seconds: 60,
            offline_after_seconds: 180,
            ..RegisterInstanceResponse::default()
        }))
    }

    async fn heartbeat(
        &self,
        _request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        self.counters.heartbeat.fetch_add(1, Ordering::Relaxed);
        Ok(Response::new(HeartbeatResponse {
            accepted: true,
            next_interval_seconds: 60,
            max_batch_bytes: 8 * 1024 * 1024,
            ..HeartbeatResponse::default()
        }))
    }

    async fn rotate_credential(
        &self,
        _request: Request<RotateCredentialRequest>,
    ) -> Result<Response<RotateCredentialResponse>, Status> {
        Err(Status::unimplemented(
            "fixture keeps the enrolled credential and never rotates it",
        ))
    }
}

impl Controller {
    async fn upload_batch(
        &self,
        request: Request<TelemetryBatch>,
    ) -> Result<Response<TelemetryReceipt>, Status> {
        let batch = request.into_inner();
        self.telemetry_counters.observe(&batch);
        let mode = self.effective_telemetry_mode()?;
        match mode {
            TelemetryMode::Unavailable => {
                self.telemetry_counters
                    .unavailable
                    .fetch_add(1, Ordering::Relaxed);
                Err(Status::unavailable(
                    "fixture telemetry backend unavailable; retry the retained batch",
                ))
            }
            TelemetryMode::Unimplemented => {
                self.telemetry_counters
                    .unimplemented
                    .fetch_add(1, Ordering::Relaxed);
                Err(Status::unimplemented(
                    "fixture does not serve telemetry uploads",
                ))
            }
            TelemetryMode::Success => {
                let store = self
                    .telemetry_store
                    .as_ref()
                    .ok_or_else(|| Status::failed_precondition("no durable telemetry store"))?;
                self.telemetry_counters.begin_inflight();
                let result = store.append(&batch);
                self.telemetry_counters.end_inflight();
                match result {
                    Ok(_) => {
                        self.telemetry_counters
                            .success
                            .fetch_add(1, Ordering::Relaxed);
                        self.telemetry_counters.ack(&batch);
                        Ok(Response::new(TelemetryReceipt {
                            batch_id: batch.batch_id,
                            sequence: batch.sequence,
                            backend: "direct".into(),
                            durable_at: now_secs() as i64,
                        }))
                    }
                    Err(err) => {
                        self.telemetry_counters
                            .store_errors
                            .fetch_add(1, Ordering::Relaxed);
                        self.record_error(format!("telemetry store: {err:#}"));
                        Err(Status::unavailable(format!(
                            "fixture telemetry store did not durably accept the batch: {err:#}"
                        )))
                    }
                }
            }
        }
    }
}

/// The production proto build is client-only (`build_server(false)`), so the
/// fixture supplies the one unary server method by hand instead of changing
/// production build settings.
#[derive(Clone)]
struct TelemetryUploadServer {
    inner: Arc<Controller>,
}

impl tonic::server::NamedService for TelemetryUploadServer {
    const NAME: &'static str = "pb.TelemetryService";
}

impl<B> tonic::codegen::Service<http::Request<B>> for TelemetryUploadServer
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<tonic::codegen::StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        if req.uri().path() == "/pb.TelemetryService/uploadBatch" {
            struct UploadBatchSvc(Arc<Controller>);
            impl tonic::server::UnaryService<TelemetryBatch> for UploadBatchSvc {
                type Response = TelemetryReceipt;
                type Future = tonic::codegen::BoxFuture<
                    tonic::Response<Self::Response>,
                    tonic::Status,
                >;
                fn call(
                    &mut self,
                    request: tonic::Request<TelemetryBatch>,
                ) -> Self::Future {
                    let inner = Arc::clone(&self.0);
                    Box::pin(async move { inner.upload_batch(request).await })
                }
            }
            Box::pin(async move {
                let method = UploadBatchSvc(inner);
                let codec = tonic_prost::ProstCodec::default();
                // The Edge compresses telemetry requests with gzip; accept it
                // exactly like the generated NodeControl server does.
                let mut accept =
                    tonic::codegen::EnabledCompressionEncodings::default();
                accept.enable(tonic::codegen::CompressionEncoding::Gzip);
                let mut grpc = tonic::server::Grpc::new(codec)
                    .apply_compression_config(
                        accept,
                        tonic::codegen::EnabledCompressionEncodings::default(),
                    );
                Ok(grpc.unary(method, req).await)
            })
        } else {
            Box::pin(async move {
                let mut response = http::Response::new(tonic::body::Body::empty());
                *response.status_mut() = http::StatusCode::OK;
                response.headers_mut().insert(
                    "content-type",
                    http::HeaderValue::from_static("application/grpc"),
                );
                response
                    .headers_mut()
                    .insert("grpc-status", http::HeaderValue::from_static("12"));
                response.headers_mut().insert(
                    "grpc-message",
                    http::HeaderValue::from_static("unimplemented"),
                );
                Ok::<_, std::convert::Infallible>(response)
            })
        }
    }
}

impl Controller {
    fn instance_id(&self) -> String {
        self.tls
            .get("instanceId")
            .and_then(Value::as_str)
            .unwrap_or(TEST_INSTANCE)
            .to_string()
    }
}

fn envelope(version: i64) -> EnvelopeMeta {
    EnvelopeMeta {
        schema_version: "node.v1".into(),
        min_reader_version: "node.v1".into(),
        base_version: version,
        patch_seq: 0,
    }
}

// ---------------------------------------------------------------------------
// Setup helpers

pub struct Certs {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn atomic_write_json(path: &Path, value: &Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

fn reserve_port() -> anyhow::Result<u16> {
    let listener = StdTcpListener::bind(("0.0.0.0", 0))?;
    Ok(listener.local_addr()?.port())
}

fn openssl() -> anyhow::Result<PathBuf> {
    for candidate in ["/usr/bin/openssl", "/usr/local/bin/openssl", "/opt/homebrew/bin/openssl"] {
        if Path::new(candidate).is_file() {
            return Ok(PathBuf::from(candidate));
        }
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("openssl");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!("openssl is required to generate throwaway fixture certificates")
}

fn run_openssl(openssl: &Path, args: &[String]) -> anyhow::Result<()> {
    let output = Command::new(openssl)
        .args(args)
        .output()
        .with_context(|| format!("run {} {}", openssl.display(), args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "openssl {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub fn resolve_certs(opts: &Options) -> anyhow::Result<Certs> {
    if let (Some(ca), Some(cert), Some(key)) =
        (&opts.ca_file, &opts.cert_file, &opts.key_file)
    {
        for path in [ca, cert, key] {
            anyhow::ensure!(path.is_file(), "certificate file missing: {}", path.display());
        }
        return Ok(Certs {
            ca: ca.clone(),
            cert: cert.clone(),
            key: key.clone(),
        });
    }
    let out_dir = opts
        .cert_dir
        .clone()
        .unwrap_or_else(|| opts.home.join("certs"));
    fs::create_dir_all(&out_dir)?;
    let ca = out_dir.join("ca.pem");
    let cert = out_dir.join("server.pem");
    let key = out_dir.join("server.key");
    if ca.is_file() && cert.is_file() && key.is_file() {
        return Ok(Certs { ca, cert, key });
    }
    let openssl = openssl()?;
    let ca_key = out_dir.join("ca.key");
    let csr = out_dir.join("server.csr");
    let ext = out_dir.join("server.ext");
    let serial = out_dir.join("ca.srl");
    fs::write(
        &ext,
        "basicConstraints=critical,CA:FALSE\n\
         keyUsage=critical,digitalSignature,keyEncipherment\n\
         extendedKeyUsage=serverAuth\n\
         subjectAltName=DNS:localhost,DNS:edge.issue34.test,IP:127.0.0.1\n",
    )?;
    let host = &opts.tls_host;
    run_openssl(
        &openssl,
        &[
            "req".into(),
            "-x509".into(),
            "-newkey".into(),
            "rsa:2048".into(),
            "-sha256".into(),
            "-days".into(),
            "2".into(),
            "-nodes".into(),
            "-subj".into(),
            "/CN=Zelvik Issue34 Test CA".into(),
            "-addext".into(),
            "basicConstraints=critical,CA:TRUE".into(),
            "-addext".into(),
            "keyUsage=critical,keyCertSign,cRLSign".into(),
            "-keyout".into(),
            ca_key.display().to_string(),
            "-out".into(),
            ca.display().to_string(),
        ],
    )?;
    run_openssl(
        &openssl,
        &[
            "req".into(),
            "-newkey".into(),
            "rsa:2048".into(),
            "-sha256".into(),
            "-nodes".into(),
            "-subj".into(),
            format!("/CN={host}"),
            "-keyout".into(),
            key.display().to_string(),
            "-out".into(),
            csr.display().to_string(),
        ],
    )?;
    run_openssl(
        &openssl,
        &[
            "x509".into(),
            "-req".into(),
            "-in".into(),
            csr.display().to_string(),
            "-CA".into(),
            ca.display().to_string(),
            "-CAkey".into(),
            ca_key.display().to_string(),
            "-CAcreateserial".into(),
            "-CAserial".into(),
            serial.display().to_string(),
            "-days".into(),
            "2".into(),
            "-sha256".into(),
            "-extfile".into(),
            ext.display().to_string(),
            "-out".into(),
            cert.display().to_string(),
        ],
    )?;
    run_openssl(
        &openssl,
        &[
            "verify".into(),
            "-CAfile".into(),
            ca.display().to_string(),
            "-purpose".into(),
            "sslserver".into(),
            cert.display().to_string(),
        ],
    )?;
    Ok(Certs { ca, cert, key })
}

pub fn build_snapshot(
    opts: &Options,
    certs: &Certs,
    tls_port: u16,
    origin: &str,
) -> anyhow::Result<Vec<u8>> {
    let cert_pem = fs::read_to_string(&certs.cert)?;
    let key_pem = fs::read_to_string(&certs.key)?;
    let snapshot = json!({
        "id": 1,
        "nodeId": opts.instance_id,
        "version": opts.version,
        "edition": "issue34-fixture",
        "isOn": true,
        "enableIPLists": false,
        // The fixture origin runs on loopback. Production refuses local
        // origins unless the control plane explicitly allows them, so the
        // snapshot must carry that switch rather than relying on a fallback.
        "globalServerConfig": {
            "httpAll": {
                "allowLocalOrigins": true,
                "supportsLowVersionHTTP": true
            },
            // The retry/recovery check observes real Edge telemetry calls, so
            // the fixture must actually produce access-log records.
            "httpAccessLog": {
                "isOn": true,
                "enableRequestHeaders": false,
                "enableResponseHeaders": false,
                "enableCookies": false,
                "sampleRate": 1.0
            }
        },
        "telemetry": {
            "assignmentVersion": "issue34-fixture-assignment",
            "routeVersion": "issue34-fixture-route",
            "clusterId": "1",
            "billingRegionId": "1",
        },
        "metricItems": [
            {"id": 1, "name": "fixture-day", "code": "fixture-day", "category": "http",
             "keys": ["${country}"], "value": "${countRequest}", "period": 1,
             "periodUnit": "day", "version": 1, "isOn": true},
            {"id": 2, "name": "fixture-month", "code": "fixture-month", "category": "http",
             "keys": ["${country}"], "value": "${countRequest}", "period": 1,
             "periodUnit": "month", "version": 1, "isOn": true}
        ],
        "servers": [{
            "id": 1,
            "isOn": true,
            "serverNames": [{"name": opts.tls_host}],
            "http": {"isOn": false, "listen": []},
            "https": {
                "isOn": true,
                "listen": [{
                    "host": "0.0.0.0",
                    "portRange": tls_port.to_string(),
                    "protocol": "https",
                }],
                "sslPolicy": {
                    "isOn": true,
                    "id": 1,
                    "ocsp": false,
                    "http2Enabled": true,
                    "minVersion": "TLSv1.2",
                    "certs": [{
                        "id": 1,
                        "isOn": true,
                        "isDefault": true,
                        "ocsp": false,
                        "dnsNames": [opts.tls_host],
                        "certData": cert_pem,
                        "keyData": key_pem,
                    }],
                },
            },
            "reverseProxy": {
                "isOn": true,
                "primaryOrigins": [{
                    "id": 1,
                    "isOn": true,
                    "name": "issue34-fixture-origin",
                    "addr": origin,
                    "weight": 100,
                    "requestHost": "",
                    "followHost": false,
                    "followPort": false,
                    "http2Enabled": false,
                    "http3Enabled": false,
                }],
                "scheduling": {"code": "polling", "options": {}},
            },
            "web": {"isOn": true},
        }],
    });
    Ok(serde_json::to_vec(&snapshot)?)
}

fn write_home(opts: &Options, grpc_addr: &str, snapshot: &[u8]) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(opts.home.join("configs"))?;
    fs::create_dir_all(opts.home.join("data"))?;
    fs::create_dir_all(opts.home.join("logs"))?;
    // Do not let a fixture provision over a real installed node home.
    let marker = opts.home.join(".issue34-fixture");
    if !marker.exists() {
        let existing = fs::read_dir(&opts.home)
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.file_name() != "configs" && entry.file_name() != "data" && entry.file_name() != "logs")
            })
            .unwrap_or(false);
        anyhow::ensure!(
            !existing || !opts.home.join("configs").join("api_node.yaml").exists(),
            "refusing to overwrite a non-fixture home: {} (pass an empty directory)",
            opts.home.display()
        );
        fs::write(&marker, b"issue34 full-edge fixture home\n")?;
    }
    let yaml = format!(
        "instanceId: \"{}\"\nsecret: \"{}\"\nrpc.endpoints:\n  - \"http://{}\"\ncommMode: \"plaintext\"\ngcloudNodeId: \"1\"\nrpc.disableUpdate: true\nxdp:\n  enabled: false\n",
        opts.instance_id, opts.secret, grpc_addr
    );
    atomic_write(&opts.home.join("configs").join("api_node.yaml"), yaml.as_bytes())?;
    let snapshot_path = opts.home.join("data").join("issue34-snapshot.json");
    atomic_write(&snapshot_path, snapshot)?;
    Ok(snapshot_path)
}

fn pid_file(home: &Path) -> PathBuf {
    home.join("data").join("zelvik-edge.pid")
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn wait_for_pid_file(path: &Path, child: &mut Child, timeout: Duration) -> anyhow::Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(pid) = read_pid(path) {
            return Ok(pid);
        }
        if let Some(status) = child.try_wait()? {
            bail!("Edge exited before writing {}: {status}", path.display());
        }
        if Instant::now() >= deadline {
            bail!("Edge pid file {} did not appear", path.display());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_tcp(host: &str, port: u16, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match TcpStream::connect((host, port)) {
            Ok(_) => return Ok(()),
            Err(err) => {
                last = Some(err);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    bail!(
        "Edge did not accept connections on {host}:{port}: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    )
}

fn wait_health(controller: &Controller, child: &mut Child, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if controller.health_value().is_some() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!("Edge exited before reporting NodeControl health: {status}");
        }
        if Instant::now() >= deadline {
            bail!("Edge did not report NodeControl health within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn spawn_edge(opts: &Options) -> anyhow::Result<Child> {
    let binary = opts
        .edge_binary
        .as_ref()
        .context("--edge-binary is required with --spawn-edge")?;
    let mut command = Command::new(binary);
    command
        .arg(&opts.edge_arg)
        .env("ZELVIK_EDGE_HOME", &opts.home)
        .stdin(Stdio::null());
    if let Some(log) = &opts.edge_log {
        if let Some(parent) = log.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let stderr = file.try_clone()?;
        command.stdout(Stdio::from(file)).stderr(Stdio::from(stderr));
        if file_size(log) > MAX_EDGE_LOG_BYTES {
            bail!("edge log {} is over the bounded size", log.display());
        }
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }
    command.spawn().with_context(|| {
        format!(
            "spawn the production Edge binary {}",
            binary.display()
        )
    })
}

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        if pid > 0 {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Status endpoint and origin

async fn serve_status(listener: TcpListener, controller: Arc<Controller>) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        let controller = Arc::clone(&controller);
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let mut request_line = request
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace();
            let method = request_line.next().unwrap_or_default();
            let path = request_line.next().unwrap_or("/");
            let (status, body) = if path.starts_with("/healthz") {
                ("200 OK", b"ok\n".to_vec())
            } else if path.starts_with("/status") {
                (
                    "200 OK",
                    serde_json::to_vec_pretty(&controller.status_doc()).unwrap_or_default(),
                )
            } else if path.starts_with("/telemetry-mode") && method == "POST" {
                let raw = request
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body.trim())
                    .unwrap_or_default();
                let mode_text = serde_json::from_str::<Value>(raw)
                    .ok()
                    .and_then(|value| value.get("mode").and_then(Value::as_str).map(str::to_owned))
                    .unwrap_or_else(|| raw.to_string());
                match TelemetryMode::parse(&mode_text)
                    .and_then(|mode| controller.set_telemetry_mode(mode))
                {
                    Ok(()) => (
                        "200 OK",
                        serde_json::to_vec(&json!({
                            "ok": true,
                            "telemetry": controller.telemetry_contract(),
                        }))
                        .unwrap_or_default(),
                    ),
                    Err(err) => (
                        "400 Bad Request",
                        serde_json::to_vec(&json!({"ok": false, "error": err.to_string()}))
                            .unwrap_or_default(),
                    ),
                }
            } else if path.starts_with("/telemetry-mode") {
                (
                    "200 OK",
                    serde_json::to_vec(&controller.telemetry_contract()).unwrap_or_default(),
                )
            } else {
                ("404 Not Found", b"not found\n".to_vec())
            };
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&body).await;
        });
    }
}

fn deterministic_payload() -> Vec<u8> {
    let unit = b"zelvik-issue34-origin\n";
    unit.iter().copied().cycle().take(16 * 1024).collect()
}

async fn serve_origin(listener: TcpListener) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(_) => break,
        };
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0usize;
            loop {
                if total == buf.len() {
                    return;
                }
                let n = match stream.read(&mut buf[total..]).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };
                total += n;
                if buf[..total].windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf[..total]).to_string();
            let content_length = request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if content_length > 8 * 1024 * 1024 {
                return;
            }
            let body_start = request.find("\r\n\r\n").map(|i| i + 4).unwrap_or(total);
            let mut body = buf[body_start..total].to_vec();
            while body.len() < content_length {
                let mut more = vec![0u8; content_length - body.len()];
                match stream.read(&mut more).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => body.extend_from_slice(&more[..n]),
                }
            }
            let is_post = request
                .lines()
                .next()
                .map(|line| line.starts_with("POST "))
                .unwrap_or(false);
            let response = if is_post { body } else { deterministic_payload() };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&response).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Smoke checks

fn curl_check(
    tmp: &Path,
    name: &str,
    ca: &Path,
    host: &str,
    port: u16,
    http_flag: &str,
    payload: &[u8],
) -> anyhow::Result<Value> {
    let request = tmp.join(format!("{name}-request.bin"));
    let body = tmp.join(format!("{name}-body.bin"));
    fs::write(&request, payload)?;
    let url = format!("https://{host}:{port}{PAYLOAD_PATH}");
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail-with-body",
            http_flag,
            "--cacert",
            &ca.display().to_string(),
            "--resolve",
            &format!("{host}:{port}:127.0.0.1"),
            "--data-binary",
            &format!("@{}", request.display()),
            "--output",
            &body.display().to_string(),
            "--write-out",
            "%{http_version}\t%{ssl_verify_result}\t%{http_code}\t%{size_download}",
            &url,
        ])
        .output()
        .context("curl is required for the TLS/h2 smoke check")?;
    anyhow::ensure!(
        output.status.success(),
        "curl {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().split('\t').collect();
    anyhow::ensure!(parts.len() == 4, "unexpected curl write-out: {stdout:?}");
    let version = parts[0].to_string();
    let verify: i64 = parts[1].parse()?;
    let code: u16 = parts[2].parse()?;
    let size: usize = parts[3].parse()?;
    let response = fs::read(&body)?;
    anyhow::ensure!(code == 200, "curl {name} returned HTTP {code}");
    anyhow::ensure!(verify == 0, "curl {name} ssl_verify_result={verify}");
    anyhow::ensure!(
        response == payload,
        "curl {name} payload mismatch: got {} bytes, expected {}",
        response.len(),
        payload.len()
    );
    anyhow::ensure!(size == payload.len(), "curl {name} size mismatch");
    Ok(json!({
        "name": name,
        "http_version": version,
        "ssl_verify_result": verify,
        "http_code": code,
        "payload_bytes": response.len(),
        "payload_sha256": sha256_hex(&response),
        "ca_verified": verify == 0,
        "sni_host": host,
    }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(bytes))
}

fn openssl_alpn_check(ca: &Path, host: &str, port: u16) -> anyhow::Result<Value> {
    let output = Command::new("openssl")
        .args([
            "s_client",
            "-connect",
            &format!("127.0.0.1:{port}"),
            "-servername",
            host,
            "-alpn",
            "h2",
            "-CAfile",
            &ca.display().to_string(),
            "-verify_return_error",
            "-verify_hostname",
            host,
        ])
        .stdin(Stdio::null())
        .output()
        .context("openssl is required for the ALPN/SNI check")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        text.contains("Verify return code: 0 (ok)"),
        "openssl did not verify the peer: {text}"
    );
    anyhow::ensure!(
        text.contains("ALPN protocol: h2"),
        "openssl did not negotiate h2: {text}"
    );
    Ok(json!({
        "name": "openssl-alpn",
        "alpn": "h2",
        "sni": host,
        "peer_verified": true,
        "verify_return_code": 0,
    }))
}

async fn telemetry_reject_check(grpc_addr: &str) -> anyhow::Result<Value> {
    let mut client =
        zelvik_edge::pb::telemetry_service_client::TelemetryServiceClient::connect(format!(
            "http://{grpc_addr}"
        ))
        .await?;
    let error = client
        .upload_batch(zelvik_edge::pb::TelemetryBatch::default())
        .await
        .expect_err("the fixture must not acknowledge telemetry");
    anyhow::ensure!(
        error.code() == tonic::Code::Unimplemented,
        "telemetry rejection changed: {error:?}"
    );
    Ok(json!({
        "served": false,
        "grpc_code": "UNIMPLEMENTED",
        "code_number": 12,
        "message": error.message(),
        "edge_classification": "terminal rejection; head quarantined with a durable gap",
        "acknowledged": false,
    }))
}

async fn wait_telemetry<F>(
    controller: &Controller,
    timeout: Duration,
    predicate: F,
) -> anyhow::Result<Value>
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let counters = controller.telemetry_contract()["counters"].clone();
        if predicate(&counters) {
            return Ok(counters);
        }
        if Instant::now() >= deadline {
            bail!("telemetry status did not reach the expected state: {counters}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Exercise the retryable path with the real Edge, not a direct RPC probe:
/// access-log batches produced by real TLS requests hit `UNAVAILABLE`, stay
/// pending, then recover through the durable store while the Edge PID stays
/// fixed. The controller status keeps scalars only, so the observation cannot
/// grow with the backlog.
async fn telemetry_retryable_recovery_check(
    controller: &Controller,
    tmp: &Path,
    ca: &Path,
    host: &str,
    port: u16,
    payload: &[u8],
) -> anyhow::Result<Value> {
    let started = Instant::now();
    controller.set_telemetry_mode(TelemetryMode::Unavailable)?;
    wait_telemetry(controller, Duration::from_secs(30), |counters| {
        counters["unavailable"].as_u64().unwrap_or(0) >= 1
    })
    .await
    .context("the real Edge never retried a telemetry batch against UNAVAILABLE")?;

    // More access-log windows while the backend is down: pending must grow,
    // but only as bounded counters.
    for index in 0..2 {
        curl_check(
            tmp,
            &format!("retry-load-{index}"),
            ca,
            host,
            port,
            "--http1.1",
            payload,
        )?;
        tokio::time::sleep(Duration::from_millis(1400)).await;
    }
    let before = wait_telemetry(controller, Duration::from_secs(30), |counters| {
        counters["maxSequence"].as_u64().unwrap_or(0) >= 2
            && counters["pendingBatches"].as_u64().unwrap_or(0) >= 1
    })
    .await
    .context("the real Edge did not retain multiple telemetry batches")?;
    let pending_before = before["pendingBatches"].as_u64().unwrap_or(0);
    let max_sequence_before = before["maxSequence"].as_u64().unwrap_or(0);
    let max_batch_bytes = before["maxBatchBytes"].as_u64().unwrap_or(0);
    anyhow::ensure!(pending_before >= 1, "pending batch counter did not grow");
    anyhow::ensure!(
        max_batch_bytes > 0 && max_batch_bytes <= 8 * 1024 * 1024,
        "telemetry batch size {max_batch_bytes} is outside the bounded message budget"
    );
    anyhow::ensure!(
        before
            .as_object()
            .is_some_and(|object| object.values().all(Value::is_number)),
        "telemetry counters are not all bounded scalars"
    );

    controller.set_telemetry_mode(TelemetryMode::Success)?;
    let recovered = wait_telemetry(controller, Duration::from_secs(75), |counters| {
        counters["pendingBatches"].as_u64() == Some(0)
            && counters["corePendingBatches"].as_u64() == Some(0)
            && counters["detailPendingBatches"].as_u64() == Some(0)
            && counters["success"].as_u64().unwrap_or(0) >= 1
    })
    .await
    .context("telemetry did not recover after the durable backend came back")?;
    Ok(json!({
        "name": "telemetry-retryable-recovery",
        "ok": true,
        "mode": "unavailable->success",
        "pending_before": pending_before,
        "pending_after": recovered["pendingBatches"],
        "core_pending_before": before["corePendingBatches"],
        "detail_pending_before": before["detailPendingBatches"],
        "core_pending_after": recovered["corePendingBatches"],
        "detail_pending_after": recovered["detailPendingBatches"],
        "max_pending": recovered["maxPendingBatches"],
        "max_sequence_before": max_sequence_before,
        "acked_sequence": recovered["ackedSequence"],
        "success": recovered["success"],
        "store_errors": recovered["storeErrors"],
        "max_batch_bytes": max_batch_bytes,
        "requests": recovered["requests"],
        "bounded_status": true,
        "recovered": true,
        "duration_seconds": started.elapsed().as_secs_f64(),
    }))
}

fn status_contract(doc: &Value) -> anyhow::Result<()> {
    let governor = doc
        .get("resourceGovernor")
        .and_then(Value::as_object)
        .context("controller status has no resourceGovernor object yet")?;
    for (section, field) in [
        ("workerReclaim", "generation"),
        ("workerReclaim", "requests"),
        ("workerReclaim", "started"),
        ("workerReclaim", "live"),
        ("workerReclaim", "stopped"),
        ("workerReclaim", "parkCollects"),
        ("workerReclaim", "rateLimited"),
        ("workerReclaim", "stopCollects"),
        ("workerReclaim", "unhookedPingoraWorkers"),
        ("cacheJanitor", "cycles"),
        ("cacheJanitor", "maintenanceCycles"),
        ("cacheJanitor", "admissionSkips"),
        ("cacheJanitor", "budgetPublishes"),
        ("cacheJanitor", "rebuildsDeferred"),
        ("cacheJanitor", "rebuildsApplied"),
        ("cacheJanitor", "negativeEntriesRemoved"),
        ("cacheJanitor", "workerCollectRequests"),
        ("metricValuesCache", "entries"),
        ("metricValuesCache", "estimatedBytes"),
        ("metricValuesCache", "maxEntries"),
        ("metricValuesCache", "maxEstimatedBytes"),
        ("metricValuesCache", "evictedEntries"),
    ] {
        let value = governor
            .get(section)
            .and_then(|value| value.get(field))
            .and_then(Value::as_u64)
            .with_context(|| {
                format!("resourceGovernor.{section}.{field} is missing or not numeric")
            })?;
        let _ = value;
    }
    governor
        .get("metricsAggregatorBytes")
        .and_then(Value::as_u64)
        .context("resourceGovernor.metricsAggregatorBytes is missing or not numeric")?;
    for field in ["configServers", "configReloadGeneration"] {
        let value = governor
            .get(field)
            .and_then(Value::as_u64)
            .with_context(|| format!("resourceGovernor.{field} is missing or not numeric"))?;
        anyhow::ensure!(
            value >= 1,
            "resourceGovernor.{field}={value}, expected >= 1 (snapshot was not applied)"
        );
    }
    let covers = governor
        .get("workerReclaim")
        .and_then(|v| v.get("coversAllWorkers"))
        .and_then(Value::as_bool)
        .context("resourceGovernor.workerReclaim.coversAllWorkers is missing")?;
    let _ = covers;
    Ok(())
}

// ---------------------------------------------------------------------------
// Main fixture flow

fn publish_fixture_ready(
    path: &Option<PathBuf>,
    handle: &Value,
    status: &str,
    edge_pid: Option<u32>,
    controller: &Controller,
) -> anyhow::Result<()> {
    let mut doc = handle.clone();
    doc["status"] = json!(status);
    doc["edge_pid"] = json!(edge_pid);
    doc["counters"] = controller.counters.json();
    doc["resourceGovernor"] = controller.status_doc()["resourceGovernor"].clone();
    doc["updatedAt"] = json!(now_secs());
    if let Some(path) = path {
        atomic_write_json(path, &doc)?;
    }
    println!("{}", serde_json::to_string(&doc)?);
    use std::io::Write;
    std::io::stdout().flush().ok();
    Ok(())
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[tokio::test]
    async fn durable_fixture_receipt_retires_the_real_edge_spool() -> anyhow::Result<()> {
        let home = std::env::temp_dir().join(format!("issue34-receipt-{}", uuid::Uuid::new_v4()));
        let spool = zelvik_edge::telemetry::Spool::open(&home.join("spool"), true, 4 << 20)?;
        spool.append("unique_ip", vec![], 120, false, 180)?;
        let batch = spool.at(0)?.expect("pending batch");
        let controller = Controller {
            home: home.clone(),
            pid_file: home.join("edge.pid"),
            secret: String::new(),
            version: 1,
            snapshot: vec![],
            tls: Value::Null,
            status_file: None,
            counters: Counters::default(),
            telemetry_mode: Mutex::new(TelemetryMode::Success),
            telemetry_mode_file: None,
            telemetry_store: Some(Arc::new(TelemetryStore::new(
                home.join("accepted.bin"),
                4 << 20,
            ))),
            telemetry_counters: TelemetryCounters::default(),
            health: Mutex::new(None),
            errors: Mutex::new(VecDeque::new()),
        };
        let receipt = controller
            .upload_batch(Request::new(batch))
            .await?
            .into_inner();
        assert!(home.join("accepted.bin").metadata()?.len() > 0);
        spool.acknowledge(&receipt)?;
        assert_eq!(spool.snapshot()["pendingBatches"], 0);
        assert_eq!(spool.snapshot()["pendingBytes"], 0);
        drop(spool);
        fs::remove_dir_all(home)?;
        Ok(())
    }
}

pub async fn run(opts: Options) -> anyhow::Result<Value> {
    let started = Instant::now();
    let home = opts.home.clone();
    fs::create_dir_all(&home)?;
    // A smoke run always has a durable store so it can prove recovery from
    // retryable telemetry failures with a real receipt, not a fake ack.
    let telemetry_store_path = opts
        .telemetry_store
        .clone()
        .or_else(|| opts.smoke.then(|| home.join("data").join("telemetry-store.bin")));
    let certs = resolve_certs(&opts)?;

    let mut origin_task = None;
    let origin = if opts.origin == "auto" {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        origin_task = Some(tokio::spawn(serve_origin(listener)));
        format!("http://{addr}")
    } else {
        opts.origin.clone()
    };

    let tls_port = if opts.tls_port == 0 {
        reserve_port()?
    } else {
        opts.tls_port
    };
    let snapshot = build_snapshot(&opts, &certs, tls_port, &origin)?;
    let snapshot_sha = sha256_hex(&snapshot);

    let grpc_listener = TcpListener::bind(&opts.grpc_listen).await?;
    let grpc_addr = grpc_listener.local_addr()?.to_string();
    let snapshot_path = write_home(&opts, &grpc_addr, &snapshot)?;

    let status_listener = if opts.status_listen == "off" {
        None
    } else {
        Some(TcpListener::bind(&opts.status_listen).await?)
    };
    let status_url = status_listener
        .as_ref()
        .map(|listener| listener.local_addr().map(|addr| format!("http://{addr}/status")))
        .transpose()?;

    let telemetry_mode = TelemetryMode::parse(&opts.telemetry_mode)?;
    let telemetry_store = telemetry_store_path
        .clone()
        .map(|path| Arc::new(TelemetryStore::new(path, opts.telemetry_store_max_bytes)));
    let controller = Arc::new(Controller {
        home: home.clone(),
        pid_file: pid_file(&home),
        secret: opts.secret.clone(),
        version: opts.version,
        snapshot,
        tls: json!({
            "host": opts.tls_host,
            "port": tls_port,
            "ca": certs.ca,
            "cert": certs.cert,
            "key": certs.key,
            "instanceId": opts.instance_id,
        }),
        status_file: opts.status_file.clone(),
        counters: Counters::default(),
        telemetry_mode: Mutex::new(telemetry_mode),
        telemetry_mode_file: opts.telemetry_mode_file.clone(),
        telemetry_store,
        telemetry_counters: TelemetryCounters::default(),
        health: Mutex::new(None),
        errors: Mutex::new(VecDeque::new()),
    });
    if let Some(listener) = status_listener {
        tokio::spawn(serve_status(listener, Arc::clone(&controller)));
    }

    let node_control = NodeControlServer::from_arc(Arc::clone(&controller))
        .accept_compressed(tonic::codec::CompressionEncoding::Gzip)
        .send_compressed(tonic::codec::CompressionEncoding::Gzip);
    let control_channel = ControlChannelServer::from_arc(Arc::clone(&controller));
    let telemetry_service = TelemetryUploadServer {
        inner: Arc::clone(&controller),
    };
    let grpc_task = tokio::spawn(async move {
        if let Err(err) = Server::builder()
            .add_service(node_control)
            .add_service(control_channel)
            .add_service(telemetry_service)
            .serve_with_incoming(TcpListenerStream::new(grpc_listener))
            .await
        {
            eprintln!("issue34-controller: gRPC server stopped: {err}");
        }
    });

    // Everything the soak driver needs to point the real binary at this
    // fixture. `status` is advanced only after the fixture has observed the
    // real Edge: `controller-ready` means the fake control plane is serving,
    // `edge-ready` means a NodeControl ReportNodeHealth arrived.
    let fixture_handle = json!({
        "schema": "issue34-full-edge-fixture/v1",
        "status": "starting",
        "home": home,
        "pid_file": pid_file(&home),
        "grpc_addr": grpc_addr,
        "status_url": status_url,
        "snapshot_file": snapshot_path,
        "snapshot_sha256": snapshot_sha,
        "tls": controller.tls,
        "telemetry": controller.telemetry_contract(),
        "telemetry_mode_file": opts.telemetry_mode_file,
        "telemetry_store": telemetry_store_path,
        "origin": origin,
        "controller_pid": std::process::id(),
        "run_soak": {
            "target_cmd_kind": "full-edge",
            "target_args": [opts.edge_arg],
            "target_env": [format!("ZELVIK_EDGE_HOME={}", home.display())],
            "target_pid_file": pid_file(&home),
            "tls_host": opts.tls_host,
            "tls_port": tls_port,
            "tls_ca": certs.ca,
            "status_source": status_url,
            "telemetry_status": status_url,
            "telemetry_mode_file": opts.telemetry_mode_file,
            "telemetry_store": telemetry_store_path,
            "require_node_status": true,
        },
    });

    let mut edge = if opts.spawn_edge {
        Some(spawn_edge(&opts)?)
    } else {
        None
    };

    let mut failures: Vec<String> = Vec::new();
    let mut edge_pid = None;
    let mut checks_report = Value::Null;
    let ready_timeout = Duration::from_secs(if opts.spawn_edge { 90 } else { 600 });

    if let Some(child) = edge.as_mut() {
        match wait_for_pid_file(&pid_file(&home), child, Duration::from_secs(90)) {
            Ok(pid) => {
                edge_pid = Some(pid);
                if pid != child.id() {
                    failures.push(format!(
                        "Edge pid file says {pid}, spawned process is {}",
                        child.id()
                    ));
                }
            }
            Err(err) => failures.push(err.to_string()),
        }
        if failures.is_empty()
            && let Err(err) = wait_tcp("127.0.0.1", tls_port, Duration::from_secs(90))
        {
            failures.push(err.to_string());
        }
        if failures.is_empty()
            && opts.require_health
            && let Err(err) = wait_health(&controller, child, Duration::from_secs(60))
        {
            failures.push(err.to_string());
        }
        if failures.is_empty() && controller.health_value().is_some() {
            publish_fixture_ready(
                &opts.ready_file,
                &fixture_handle,
                "edge-ready",
                edge_pid,
                &controller,
            )?;
        }
        if failures.is_empty() && opts.smoke {
            let payload = b"issue34-full-edge-payload\n".repeat(1024);
            let tmp = home.join("smoke");
            fs::create_dir_all(&tmp)?;
            let mut smoke = Vec::new();
            // Access-log telemetry from these real TLS requests must hit a
            // retryable backend first; the recovery check flips it to success.
            if let Err(err) = controller.set_telemetry_mode(TelemetryMode::Unavailable) {
                failures.push(format!("telemetry unavailable mode: {err:#}"));
            }
            for (name, flag, want) in [
                ("http1-post", "--http1.1", "1.1"),
                ("http2-post", "--http2", "2"),
            ] {
                match curl_check(&tmp, name, &certs.ca, &opts.tls_host, tls_port, flag, &payload) {
                    Ok(mut result) => {
                        let version = result["http_version"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let ok = if want == "2" {
                            version == "2"
                        } else {
                            version.starts_with("1.")
                        };
                        result["expected_http_version"] = json!(want);
                        result["ok"] = json!(ok);
                        if !ok {
                            failures.push(format!(
                                "{name}: negotiated HTTP/{version}, expected {want}"
                            ));
                        }
                        smoke.push(result);
                    }
                    Err(err) => failures.push(format!("{name}: {err:#}")),
                }
            }
            match openssl_alpn_check(&certs.ca, &opts.tls_host, tls_port) {
                Ok(value) => smoke.push(value),
                Err(err) => failures.push(format!("openssl ALPN/SNI: {err:#}")),
            }
            match telemetry_retryable_recovery_check(
                &controller,
                &tmp,
                &certs.ca,
                &opts.tls_host,
                tls_port,
                &payload,
            )
            .await
            {
                Ok(value) => smoke.push(value),
                Err(err) => failures.push(format!("telemetry retry/recovery: {err:#}")),
            }
            if let Err(err) = controller.set_telemetry_mode(TelemetryMode::Unimplemented) {
                failures.push(format!("telemetry unimplemented mode: {err:#}"));
            }
            match telemetry_reject_check(&grpc_addr).await {
                Ok(value) => {
                    smoke.push(json!({"name": "telemetry-reject", "ok": true, "result": value}))
                }
                Err(err) => failures.push(format!("telemetry reject: {err:#}")),
            }
            let status = controller.status_doc();
            let config_applied = status
                .pointer("/resourceGovernor/configServers")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= 1
                && status
                    .pointer("/resourceGovernor/configReloadGeneration")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    >= 1;
            if let Err(err) = status_contract(&status) {
                failures.push(format!("node status contract: {err:#}"));
            } else {
                smoke.push(json!({"name": "node-status", "ok": true}));
            }
            if !config_applied {
                failures.push(
                    "the Edge reported no applied server config (configServers/configReloadGeneration)"
                        .into(),
                );
            }
            let same_pid = read_pid(&pid_file(&home)) == edge_pid;
            if !same_pid {
                failures.push("Edge pid changed during the smoke run".into());
            }
            checks_report = json!({
                "checks": smoke,
                "edge_pid": edge_pid,
                "same_pid": same_pid,
                "config_applied": config_applied,
                "status_source": status_url,
            });
        }
    } else if opts.smoke {
        failures.push("--smoke requires a spawned Edge".into());
    }

    // Serve mode: announce that the fake control plane is actually accepting
    // role RPCs *before* the driver starts the real binary. `edge-ready` is
    // published only after a real NodeControl health report, so nobody can
    // mistake a bound fixture port for a running Edge.
    if !opts.spawn_edge {
        publish_fixture_ready(
            &opts.ready_file,
            &fixture_handle,
            "controller-ready",
            None,
            &controller,
        )?;
        let deadline = Instant::now() + ready_timeout;
        loop {
            if controller.health_value().is_some() {
                edge_pid = read_pid(&pid_file(&home));
                publish_fixture_ready(
                    &opts.ready_file,
                    &fixture_handle,
                    "edge-ready",
                    edge_pid,
                    &controller,
                )?;
                break;
            }
            if Instant::now() >= deadline {
                failures.push(format!(
                    "no NodeControl health report arrived within {}s; the external Edge was not started or did not apply the snapshot",
                    ready_timeout.as_secs()
                ));
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let observed = controller.health_value().is_some();
    let current_status = controller.status_doc();
    let config_applied = current_status
        .pointer("/resourceGovernor/configServers")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        >= 1
        && current_status
            .pointer("/resourceGovernor/configReloadGeneration")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            >= 1;
    let pid_at_exit = read_pid(&pid_file(&home));
    if !observed && opts.require_health && failures.is_empty() {
        failures.push("Edge reached the fixture without a NodeControl health report".into());
    }
    if opts.require_health && edge_pid.is_some() && pid_at_exit != edge_pid {
        failures.push(format!(
            "Edge pid changed from {:?} to {:?} during the fixture run",
            edge_pid, pid_at_exit
        ));
    }

    let ready = failures.is_empty();
    let report = json!({
        "schema": "issue34-full-edge-fixture/v1",
        "status": if ready { "pass" } else { "fail" },
        "scope": {
            "component_lane": false,
            "production_startup": true,
            "full_edge": true,
            "note": "real Zelvik-Edge binary started from a fixture ZELVIK_EDGE_HOME; no bench surrogate",
        },
        "home": home,
        "pid_file": pid_file(&home),
        "grpc_addr": grpc_addr,
        "status_url": status_url,
        "snapshot_file": snapshot_path,
        "snapshot_sha256": snapshot_sha,
        "tls": controller.tls,
        "telemetry": controller.telemetry_contract(),
        "origin": origin,
        "edge_pid": edge_pid,
        "pid_at_exit": pid_at_exit,
        "node_health_observed": observed,
        "config_applied": config_applied,
        "checks": checks_report,
        "counters": controller.counters.json(),
        "status_document": controller.status_doc(),
        "failures": failures,
        "duration_seconds": started.elapsed().as_secs_f64(),
        "fixture_handle": fixture_handle.clone(),
    });

    if opts.spawn_edge && opts.smoke {
        if let Some(mut child) = edge.take() {
            terminate_child(&mut child);
        }
        grpc_task.abort();
        if let Some(task) = origin_task.take() {
            task.abort();
        }
        if let Some(path) = &opts.ready_file {
            let _ = fs::remove_file(path);
        }
        if let Some(path) = &opts.report_file {
            atomic_write_json(path, &report)?;
        }
        if opts.cleanup_home {
            let _ = fs::remove_dir_all(&home);
        }
        return Ok(report);
    }

    if ready {
        if let Some(path) = &opts.report_file {
            atomic_write_json(path, &report)?;
        }
        println!("{}", serde_json::to_string(&fixture_handle)?);
        {
            use std::io::Write;
            std::io::stdout().flush().ok();
        }
        wait_for_shutdown().await;
    } else if let Some(path) = &opts.report_file {
        atomic_write_json(path, &report)?;
    }

    if let Some(mut child) = edge.take() {
        terminate_child(&mut child);
    }
    grpc_task.abort();
    if let Some(task) = origin_task.take() {
        task.abort();
    }
    if let Some(path) = &opts.ready_file {
        let _ = fs::remove_file(path);
    }
    if opts.cleanup_home {
        let _ = fs::remove_dir_all(&home);
    }
    Ok(report)
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(not(test))]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Options::parse(std::env::args().skip(1))?;
    let report = run(opts).await?;
    let failed = report
        .get("failures")
        .and_then(Value::as_array)
        .map(|items| !items.is_empty())
        .unwrap_or(false);
    if failed {
        eprintln!("{}", serde_json::to_string_pretty(&report)?);
        std::process::exit(1);
    }
    Ok(())
}
