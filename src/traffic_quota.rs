//! Local quota windows for traffic limiting (contract:
//! `docs/contracts/edge-traffic-limit.md` in the docs repo).
//!
//! The control plane only sends `windowLimitBytes` + `periodUnit` +
//! `periodInterval`; the edge node owns window alignment, rollover and
//! accounting so enforcement stays O(1) and works while disconnected.
//!
//! Two scopes share one registry: site windows keyed by positive server id,
//! plan-instance windows keyed by the negated user plan id (same convention
//! as `traffic_shaping::ShapingScope`). Billable bytes are outbound
//! (`bytes_sent`) to match `ServerBandwidthStat` semantics; both HTTP
//! (`record::request_end*`) and L4/UDP (`record::record_transfer`) feed them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use chrono::{Datelike, Duration, NaiveDate};
use dashmap::DashMap;
use tracing::warn;

use crate::config::NodeConfig;
use crate::config_models::ServerConfig;
use crate::utils::time::{LOCAL_TZ_OFFSET_SECONDS, now_timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeriodUnit {
    Day,
    Month,
    Quarter,
    Year,
}

impl PeriodUnit {
    /// Contract parser: unknown / missing values degrade to `month`
    /// (contract default), never to a disabled quota.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "day" => Self::Day,
            "quarter" => Self::Quarter,
            "year" => Self::Year,
            _ => Self::Month,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
        }
    }
}

/// Resolved quota parameters for one window scope. A `None` spec means the
/// scope is not quota-managed (isOn=false or no positive limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaSpec {
    pub limit_bytes: u64,
    pub unit: PeriodUnit,
    /// Number of `unit`s per window; contract default 1.
    pub interval: u32,
}

impl QuotaSpec {
    fn pack(self) -> u64 {
        let unit = match self.unit {
            PeriodUnit::Day => 0u64,
            PeriodUnit::Month => 1,
            PeriodUnit::Quarter => 2,
            PeriodUnit::Year => 3,
        };
        (unit << 32) | u64::from(self.interval.max(1))
    }
}

/// Snapshot of one window for status reporting.
pub struct QuotaWindowSnapshot {
    pub window_start: i64,
    pub window_end: i64,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub unit: PeriodUnit,
    pub interval: u32,
    pub exceeded: bool,
}

/// Local-time calendar boundary of the window containing `now_ts`, aligned to
/// natural day/month/quarter/year boundaries in the node timezone. `interval`
/// periods are counted from the epoch anchor (num_days_from_ce / absolute
/// month / quarter / year index), so e.g. a quarter×1 window is always
/// Jan-Mar / Apr-Jun / Jul-Sep / Oct-Dec.
fn align_window(now_ts: i64, unit: PeriodUnit, interval: u32, tz_offset: i64) -> (i64, i64) {
    let interval = i64::from(interval.max(1));
    let local_naive = chrono::DateTime::from_timestamp(now_ts, 0)
        .unwrap_or_default()
        .naive_utc()
        + Duration::seconds(tz_offset);
    let date = local_naive.date();
    let year = i64::from(date.year());
    let month0 = i64::from(date.month0());

    let (start_date, end_date) = match unit {
        PeriodUnit::Day => {
            let days = i64::from(date.num_days_from_ce());
            let start = days.div_euclid(interval) * interval;
            (
                NaiveDate::from_num_days_from_ce_opt(start as i32),
                NaiveDate::from_num_days_from_ce_opt((start + interval) as i32),
            )
        }
        PeriodUnit::Month => {
            let idx = year * 12 + month0;
            let start = idx.div_euclid(interval) * interval;
            (
                NaiveDate::from_ymd_opt((start / 12) as i32, (start % 12 + 1) as u32, 1),
                NaiveDate::from_ymd_opt(
                    ((start + interval) / 12) as i32,
                    ((start + interval) % 12 + 1) as u32,
                    1,
                ),
            )
        }
        PeriodUnit::Quarter => {
            let idx = year * 4 + month0 / 3;
            let start = idx.div_euclid(interval) * interval;
            (
                NaiveDate::from_ymd_opt((start / 4) as i32, (start % 4 * 3 + 1) as u32, 1),
                NaiveDate::from_ymd_opt(
                    ((start + interval) / 4) as i32,
                    ((start + interval) % 4 * 3 + 1) as u32,
                    1,
                ),
            )
        }
        PeriodUnit::Year => {
            let start = year.div_euclid(interval) * interval;
            (
                NaiveDate::from_ymd_opt(start as i32, 1, 1),
                NaiveDate::from_ymd_opt((start + interval) as i32, 1, 1),
            )
        }
    };

    let to_unix = |d: Option<NaiveDate>| -> i64 {
        d.and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|dt| dt.and_utc().timestamp() - tz_offset)
            .unwrap_or(now_ts)
    };
    (to_unix(start_date), to_unix(end_date))
}

/// One lock-free quota window. Hot-path reads are plain atomic loads; the
/// only synchronization is a single CAS spin guard held briefly during the
/// rare boundary rollover.
pub struct QuotaWindow {
    window_start: AtomicI64,
    window_end: AtomicI64,
    used_bytes: AtomicU64,
    limit_bytes: AtomicU64,
    /// Packed QuotaSpec (unit<<32 | interval) so the period definition can be
    /// swapped atomically with one store.
    spec_bits: AtomicU64,
    rolling: AtomicBool,
    /// Throttles clock-rewind warnings (unix ts of last emit).
    last_rewind_warn: AtomicI64,
    unit: PeriodUnit,
}

impl QuotaWindow {
    /// Constructor for benches (the registry path requires a full NodeConfig).
    #[doc(hidden)]
    pub fn new_for_bench(spec: QuotaSpec, now: i64) -> Self {
        Self::new(spec, now)
    }

    fn new(spec: QuotaSpec, now: i64) -> Self {
        let (start, end) = align_window(
            now,
            spec.unit,
            spec.interval,
            i64::from(LOCAL_TZ_OFFSET_SECONDS.load(Ordering::Relaxed)),
        );
        Self {
            window_start: AtomicI64::new(start),
            window_end: AtomicI64::new(end),
            used_bytes: AtomicU64::new(0),
            limit_bytes: AtomicU64::new(spec.limit_bytes),
            spec_bits: AtomicU64::new(spec.pack()),
            rolling: AtomicBool::new(false),
            last_rewind_warn: AtomicI64::new(0),
            unit: spec.unit,
        }
    }

    fn spec(&self) -> QuotaSpec {
        let bits = self.spec_bits.load(Ordering::Acquire);
        QuotaSpec {
            limit_bytes: self.limit_bytes.load(Ordering::Relaxed),
            unit: self.unit,
            interval: (bits & 0xFFFF_FFFF) as u32,
        }
    }

    /// Called by config sync when the scope's spec is (re)published. A period
    /// definition change re-aligns the window and resets usage; a pure limit
    /// change keeps accumulated usage.
    fn configure(&self, spec: QuotaSpec, now: i64) {
        let bits = spec.pack();
        if self.spec_bits.swap(bits, Ordering::AcqRel) != bits {
            let (start, end) = align_window(
                now,
                spec.unit,
                spec.interval,
                i64::from(LOCAL_TZ_OFFSET_SECONDS.load(Ordering::Relaxed)),
            );
            self.used_bytes.store(0, Ordering::Relaxed);
            self.window_start.store(start, Ordering::Release);
            self.window_end.store(end, Ordering::Release);
        }
        self.limit_bytes.store(spec.limit_bytes, Ordering::Relaxed);
    }

    /// Rolls the window forward if `now` crossed `window_end`. Clock rewinds
    /// never roll the window back (plan §7): the current window is kept and a
    /// throttled warning is emitted.
    fn ensure_current(&self, now: i64) {
        let start = self.window_start.load(Ordering::Acquire);
        let end = self.window_end.load(Ordering::Acquire);
        if now >= start && now < end {
            return;
        }
        if now < start {
            let last = self.last_rewind_warn.load(Ordering::Relaxed);
            if now.saturating_sub(last) > 3600 {
                self.last_rewind_warn.store(now, Ordering::Relaxed);
                warn!(
                    "traffic_quota: clock rewound before window start (now={} start={}); keeping window",
                    now, start
                );
            }
            return;
        }
        if self
            .rolling
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Another thread is rolling; the next request sees the fresh window.
            return;
        }
        let end = self.window_end.load(Ordering::Acquire);
        if now >= end {
            let spec = self.spec();
            let (start, end) = align_window(
                now,
                spec.unit,
                spec.interval,
                i64::from(LOCAL_TZ_OFFSET_SECONDS.load(Ordering::Relaxed)),
            );
            self.used_bytes.store(0, Ordering::Relaxed);
            self.window_start.store(start, Ordering::Release);
            self.window_end.store(end, Ordering::Release);
        }
        self.rolling.store(false, Ordering::Release);
    }

    /// Hot path: single compare after a (cheap) freshness check. No
    /// allocation, no calendar math, no lock on the request path.
    pub fn exceeded(&self, now: i64) -> bool {
        self.ensure_current(now);
        let limit = self.limit_bytes.load(Ordering::Relaxed);
        limit > 0 && self.used_bytes.load(Ordering::Relaxed) >= limit
    }

    /// Account outbound bytes. Rollover happens on the same call so a
    /// boundary-crossing request lands in the new window.
    pub fn add(&self, bytes: u64, now: i64) {
        if bytes == 0 {
            return;
        }
        self.ensure_current(now);
        self.used_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn snapshot(&self, now: i64) -> QuotaWindowSnapshot {
        self.ensure_current(now);
        QuotaWindowSnapshot {
            window_start: self.window_start.load(Ordering::Relaxed),
            window_end: self.window_end.load(Ordering::Relaxed),
            used_bytes: self.used_bytes.load(Ordering::Relaxed),
            limit_bytes: self.limit_bytes.load(Ordering::Relaxed),
            unit: self.unit,
            interval: (self.spec_bits.load(Ordering::Relaxed) & 0xFFFF_FFFF) as u32,
            exceeded: self.exceeded(now),
        }
    }
}

/// Registry key: positive = site scope (server id), negative = plan-instance
/// scope (negated user plan id). Matches ShapingScope's Site=id / Plan=-id.
fn plan_key(user_plan_id: i64) -> i64 {
    -user_plan_id
}

static WINDOWS: LazyLock<DashMap<i64, Arc<QuotaWindow>>> =
    LazyLock::new(|| DashMap::with_shard_amount(32));
/// server id -> user plan id, rebuilt on every config sync so byte accounting
/// can reach the plan window without the ServerConfig in hand.
static SERVER_PLAN: LazyLock<DashMap<i64, i64>> = LazyLock::new(DashMap::new);

fn site_spec(server: &ServerConfig) -> Option<QuotaSpec> {
    server.traffic_limit.as_ref()?.quota_spec()
}

/// Reconciles the window registry with the freshly applied config. Called
/// while holding the NodeConfig write lock from every path that mutates
/// `all_servers` or `plan_derived`; DashMap is a leaf lock so this is safe.
pub fn sync_from_config(lock: &NodeConfig) {
    let now = now_timestamp();
    let mut desired: HashMap<i64, QuotaSpec> = HashMap::new();
    let mut server_plan: HashMap<i64, i64> = HashMap::new();

    for server in &lock.all_servers {
        let id = server.numeric_id();
        if id <= 0 {
            continue;
        }
        server_plan.insert(id, server.user_plan_id);
        if let Some(spec) = site_spec(server) {
            desired.insert(id, spec);
        }
    }
    for (&user_plan_id, derived) in &lock.plan_derived {
        if user_plan_id > 0
            && let Some(spec) = derived.traffic_quota
        {
            desired.insert(plan_key(user_plan_id), spec);
        }
    }

    for (key, spec) in desired.iter() {
        match WINDOWS.entry(*key) {
            dashmap::Entry::Occupied(entry) => entry.get().configure(*spec, now),
            dashmap::Entry::Vacant(entry) => {
                entry.insert(Arc::new(QuotaWindow::new(*spec, now)));
            }
        }
    }
    WINDOWS.retain(|key, _| desired.contains_key(key));

    SERVER_PLAN.clear();
    for (server_id, user_plan_id) in server_plan {
        SERVER_PLAN.insert(server_id, user_plan_id);
    }
}

/// Byte accounting hook: called from `metrics::record` with outbound bytes
/// after each request / transfer flush.
pub fn record_outbound(server_id: i64, bytes_sent: u64) {
    if bytes_sent == 0 || server_id <= 0 {
        return;
    }
    let now = now_timestamp();
    if let Some(window) = WINDOWS.get(&server_id) {
        window.add(bytes_sent, now);
    }
    if let Some(user_plan_id) = SERVER_PLAN.get(&server_id).map(|v| *v)
        && user_plan_id > 0
        && let Some(window) = WINDOWS.get(&plan_key(user_plan_id))
    {
        window.add(bytes_sent, now);
    }
}

/// Enforcement check: true when the site's own window or its plan-instance
/// window is exhausted. Sites without quota config are never affected.
pub fn quota_exceeded(server: &ServerConfig) -> bool {
    let now = now_timestamp();
    let id = server.numeric_id();
    if id > 0
        && let Some(window) = WINDOWS.get(&id)
        && window.exceeded(now)
    {
        return true;
    }
    if server.user_plan_id > 0
        && let Some(window) = WINDOWS.get(&plan_key(server.user_plan_id))
        && window.exceeded(now)
    {
        return true;
    }
    false
}

/// statusJSON payload: per-site and per-plan-instance window state for
/// control-plane aggregation.
pub fn status_snapshot_json() -> serde_json::Value {
    let now = now_timestamp();
    let mut sites = serde_json::Map::new();
    let mut plans = serde_json::Map::new();
    for entry in WINDOWS.iter() {
        let key = *entry.key();
        let snap = entry.value().snapshot(now);
        let item = serde_json::json!({
            "windowStart": snap.window_start,
            "windowEnd": snap.window_end,
            "usedBytes": snap.used_bytes,
            "limitBytes": snap.limit_bytes,
            "periodUnit": snap.unit.as_str(),
            "periodInterval": snap.interval,
            "exceeded": snap.exceeded,
        });
        if key > 0 {
            sites.insert(key.to_string(), item);
        } else {
            plans.insert((-key).to_string(), item);
        }
    }
    serde_json::json!({ "sites": sites, "plans": plans })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Asia/Shanghai: no DST, matches production fleet.
    const TZ: i64 = 8 * 3600;

    fn ts(y: i32, m: u32, d: u32, h: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
            - TZ
    }

    #[test]
    fn month_window_aligns_to_natural_month() {
        let (s, e) = align_window(ts(2026, 10, 10, 9), PeriodUnit::Month, 1, TZ);
        assert_eq!(s, ts(2026, 10, 1, 0));
        assert_eq!(e, ts(2026, 11, 1, 0));
    }

    #[test]
    fn quarter_window_aligns_to_calendar_quarter() {
        let (s, e) = align_window(ts(2026, 11, 30, 23), PeriodUnit::Quarter, 1, TZ);
        assert_eq!(s, ts(2026, 10, 1, 0));
        assert_eq!(e, ts(2027, 1, 1, 0));
    }

    #[test]
    fn year_window_aligns_to_jan_first() {
        let (s, e) = align_window(ts(2026, 7, 1, 0), PeriodUnit::Year, 1, TZ);
        assert_eq!(s, ts(2026, 1, 1, 0));
        assert_eq!(e, ts(2027, 1, 1, 0));
    }

    #[test]
    fn day_window_aligns_to_local_midnight() {
        let (s, e) = align_window(ts(2026, 10, 10, 9), PeriodUnit::Day, 1, TZ);
        assert_eq!(s, ts(2026, 10, 10, 0));
        assert_eq!(e, ts(2026, 10, 11, 0));
    }

    #[test]
    fn month_interval_n_covers_n_months() {
        // 3-month period: Oct 2026 falls into the Oct-Dec window anchored at
        // the epoch-aligned sequence (Oct is a multiple of 3 in month index).
        let (s, e) = align_window(ts(2026, 10, 10, 0), PeriodUnit::Month, 3, TZ);
        assert_eq!(s, ts(2026, 10, 1, 0));
        assert_eq!(e, ts(2027, 1, 1, 0));
        let (s, e) = align_window(ts(2026, 11, 15, 0), PeriodUnit::Month, 3, TZ);
        assert_eq!(s, ts(2026, 10, 1, 0));
        assert_eq!(e, ts(2027, 1, 1, 0));
    }

    #[test]
    fn rollover_resets_usage_at_boundary() {
        let spec = QuotaSpec {
            limit_bytes: 100,
            unit: PeriodUnit::Day,
            interval: 1,
        };
        let w = QuotaWindow::new(spec, ts(2026, 10, 10, 9));
        w.add(120, ts(2026, 10, 10, 10));
        assert!(w.exceeded(ts(2026, 10, 10, 10)));
        // Next day: window rolled, usage reset.
        assert!(!w.exceeded(ts(2026, 10, 11, 1)));
        let snap = w.snapshot(ts(2026, 10, 11, 1));
        assert_eq!(snap.used_bytes, 0);
        assert_eq!(snap.window_start, ts(2026, 10, 11, 0));
    }

    #[test]
    fn clock_rewind_never_rolls_window_back() {
        let spec = QuotaSpec {
            limit_bytes: 100,
            unit: PeriodUnit::Day,
            interval: 1,
        };
        let w = QuotaWindow::new(spec, ts(2026, 10, 10, 9));
        w.add(50, ts(2026, 10, 10, 10));
        // Clock jumps backwards into the previous window: usage and
        // boundaries are kept.
        assert!(!w.exceeded(ts(2026, 10, 9, 10)));
        let snap = w.snapshot(ts(2026, 10, 9, 10));
        assert_eq!(snap.used_bytes, 50);
        assert_eq!(snap.window_start, ts(2026, 10, 10, 0));
    }

    #[test]
    fn limit_change_keeps_usage_period_change_resets() {
        let spec = QuotaSpec {
            limit_bytes: 100,
            unit: PeriodUnit::Day,
            interval: 1,
        };
        let w = QuotaWindow::new(spec, ts(2026, 10, 10, 9));
        w.add(80, ts(2026, 10, 10, 10));
        w.configure(
            QuotaSpec {
                limit_bytes: 200,
                ..spec
            },
            ts(2026, 10, 10, 10),
        );
        assert!(!w.exceeded(ts(2026, 10, 10, 10)));
        w.configure(
            QuotaSpec {
                unit: PeriodUnit::Month,
                ..spec
            },
            ts(2026, 10, 10, 10),
        );
        let snap = w.snapshot(ts(2026, 10, 10, 10));
        assert_eq!(snap.used_bytes, 0);
        assert_eq!(snap.window_start, ts(2026, 10, 1, 0));
    }

    #[test]
    fn unknown_unit_degrades_to_month() {
        assert_eq!(PeriodUnit::parse(""), PeriodUnit::Month);
        assert_eq!(PeriodUnit::parse("fortnight"), PeriodUnit::Month);
        assert_eq!(PeriodUnit::parse("Quarter"), PeriodUnit::Quarter);
    }
}
