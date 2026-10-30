//! Immutable metric collection plan compiled from one configuration
//! generation.
//!
//! The plan is published together with the metric-item snapshot and captured
//! by every event at enqueue time. A configuration reload therefore cannot
//! retroactively apply new items, windows or key templates to samples that were
//! accepted under the old generation. Only variables that an enabled item
//! actually references are extracted from the request; an empty plan collects
//! nothing.

use arc_swap::ArcSwap;
use chrono::{Datelike, Duration as ChronoDuration, FixedOffset, TimeZone, Timelike, Weekday};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use super::projection::{
    MetricProjectionAggregator, ProjectionCounters, ProjectionKey, ResolvedDimensions,
};
use crate::config_models::MetricItemConfig;

/// Expressions a metric item can upload. They are compiled once so the request
/// path never re-parses a JSON value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricValueKind {
    Count,
    BytesSent,
    BytesReceived,
    AttackCount,
}

impl MetricValueKind {
    fn compile(value: &Value) -> Self {
        match value.as_str() {
            Some("${bytesSent}") | Some("${countTrafficOut}") => Self::BytesSent,
            Some("${countTrafficIn}") => Self::BytesReceived,
            Some("${countAttackRequest}") => Self::AttackCount,
            Some("${countRequest}") | Some("${countConnection}") => Self::Count,
            _ => Self::Count,
        }
    }

    pub fn select(self, counters: &ProjectionCounters) -> f32 {
        match self {
            Self::BytesSent => counters.bytes_sent as f32,
            Self::BytesReceived => counters.bytes_received as f32,
            Self::AttackCount => counters.count_attack as f32,
            Self::Count => counters.count as f32,
        }
    }
}

#[derive(Debug)]
pub struct CompiledMetricItem {
    pub id: i64,
    pub version: i32,
    pub category: Arc<str>,
    pub period: i32,
    pub period_unit: Arc<str>,
    pub value: MetricValueKind,
    pub keys: Arc<[Arc<str>]>,
}

impl CompiledMetricItem {
    pub fn time_key(&self, event_time_millis: i64) -> (Arc<str>, i64, i64) {
        let wall = crate::utils::time::local_from_timestamp_millis(event_time_millis);
        period_window(self.period, self.period_unit.as_ref(), wall)
    }
}

/// The plan plus the generation-owned aggregation map.
///
/// Retired plans stay reachable until their in-flight events and accepted
/// projections have been drained; the reporter removes them only when both
/// counters reach zero.
pub struct MetricCollectionPlan {
    pub generation: u64,
    fingerprint: u64,
    items: Arc<[CompiledMetricItem]>,
    aggregator: Option<Arc<MetricProjectionAggregator>>,
    http: CategoryPlan,
    tcp: CategoryPlan,
    udp: CategoryPlan,
    inflight_events: AtomicUsize,
}

#[derive(Default)]
struct CategoryPlan {
    items: usize,
    /// Request-context variables referenced by the category's key templates.
    /// Geo/browser values are derived from the analysis result by the worker,
    /// so they never need a request attribute copy.
    request_vars: BTreeSet<Arc<str>>,
}

/// One request completion as the projection layer consumes it. Grouped in a
/// struct because the event carries several independent request facts.
pub struct ProjectionEvent<'a> {
    pub category: &'a str,
    pub server_id: i64,
    pub event_time_millis: i64,
    pub attrs: &'a BTreeMap<String, String>,
    pub dimensions: &'a ResolvedDimensions,
    pub bytes_sent: i64,
    pub bytes_received: i64,
    pub is_attack: bool,
}

impl MetricCollectionPlan {
    #[cfg(test)]
    pub(crate) fn for_test(items: &[MetricItemConfig]) -> Self {
        compile_plan(items, 1, 0)
    }

    fn empty() -> Self {
        Self {
            generation: 0,
            fingerprint: 0,
            items: Arc::from([]),
            aggregator: None,
            http: CategoryPlan::default(),
            tcp: CategoryPlan::default(),
            udp: CategoryPlan::default(),
            inflight_events: AtomicUsize::new(0),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn enabled_items(&self) -> usize {
        self.items.len()
    }

    pub fn has_items_for(&self, category: &str) -> bool {
        self.category_plan(category).items > 0
    }

    /// Request variables the category's templates need. `None` means no event
    /// of that category can produce a custom metric, so no request context
    /// should be built at all.
    pub fn request_vars_for(&self, category: &str) -> Option<&BTreeSet<Arc<str>>> {
        let plan = self.category_plan(category);
        (!plan.request_vars.is_empty()).then_some(&plan.request_vars)
    }

    fn category_plan(&self, category: &str) -> &CategoryPlan {
        match category {
            crate::metrics::METRIC_CATEGORY_TCP => &self.tcp,
            crate::metrics::METRIC_CATEGORY_UDP => &self.udp,
            _ => &self.http,
        }
    }

    pub fn aggregator(&self) -> Option<&Arc<MetricProjectionAggregator>> {
        self.aggregator.as_ref()
    }

    /// Item definition frozen with this generation. The reporter uses it to
    /// choose the uploaded value; a retired plan keeps its own definition even
    /// after the live config has moved on.
    pub fn item(&self, item_id: i64, version: i32) -> Option<&CompiledMetricItem> {
        self.items
            .iter()
            .find(|item| item.id == item_id && item.version == version)
    }

    /// Count an event that was queued while this plan was current. The reporter
    /// may not retire the plan until the event has been consumed.
    pub fn acquire_event(&self) {
        self.inflight_events.fetch_add(1, Ordering::AcqRel);
    }

    pub fn release_event(&self) {
        let _ = self
            .inflight_events
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_sub(1)
            });
    }

    pub fn inflight_events(&self) -> usize {
        self.inflight_events.load(Ordering::Acquire)
    }

    /// True when a retired plan has no accepted data and no queued event left.
    pub fn is_drained(&self) -> bool {
        self.inflight_events() == 0
            && self
                .aggregator
                .as_ref()
                .is_none_or(|aggregator| aggregator.is_empty())
    }

    /// Project one request event into the enabled items of its category.
    ///
    /// Returns the number of projections that were dropped because a
    /// cardinality or byte ceiling refused the new key. A dropped projection is
    /// never partially counted: either the whole event is charged and added, or
    /// it is reported as dropped.
    pub fn record_event(&self, event: ProjectionEvent<'_>) -> usize {
        let ProjectionEvent {
            category,
            server_id,
            event_time_millis,
            attrs,
            dimensions,
            bytes_sent,
            bytes_received,
            is_attack,
        } = event;
        let Some(aggregator) = self.aggregator.as_ref() else {
            return 0;
        };
        let mut dropped = 0;
        for item in self
            .items
            .iter()
            .filter(|item| item.category.as_ref() == category)
        {
            let (time_key, window_start, window_end) = item.time_key(event_time_millis);
            let keys = item
                .keys
                .iter()
                .map(|template| resolve_projection_key(template, attrs, dimensions))
                .collect::<Vec<_>>();
            let key = ProjectionKey {
                item_id: item.id,
                version: item.version,
                time_key,
                window_start,
                window_end,
                server_id,
                keys: Arc::from(keys),
            };
            let delta = ProjectionCounters {
                count: 1,
                count_attack: i64::from(is_attack),
                bytes_sent,
                bytes_received,
                attack_bytes: if is_attack { bytes_sent } else { 0 },
            };
            if !aggregator.record(key, delta, event_time_millis) {
                dropped += 1;
            }
        }
        dropped
    }
}

/// A request-scoped hold on one plan generation. Dropping the request context
/// releases the hold, which lets the reporter retire the generation once the
/// completion event has also been consumed. This must be held from request
/// start (not only from event enqueue) so a reload cannot retire a generation
/// that a still-running request will later record into.
pub struct MetricPlanLease {
    plan: Arc<MetricCollectionPlan>,
}

impl MetricPlanLease {
    /// Only [`current_metric_plan_leased`] may create a lease: it holds the
    /// lifecycle lock across load-and-acquire so retirement cannot slip in
    /// between.
    fn new(plan: Arc<MetricCollectionPlan>) -> Self {
        plan.acquire_event();
        Self { plan }
    }

    pub fn plan(&self) -> &Arc<MetricCollectionPlan> {
        &self.plan
    }
}

impl Drop for MetricPlanLease {
    fn drop(&mut self) {
        self.plan.release_event();
    }
}

/// Variables supplied by the analysis result instead of the request context.
fn is_analysis_variable(name: &str) -> bool {
    matches!(
        name,
        "country"
            | "geo.country.name"
            | "province"
            | "geo.province.name"
            | "city"
            | "geo.city.name"
            | "provider"
            | "isp.name"
            | "browser"
            | "browser.name"
            | "browser.version"
            | "os"
            | "browser.os.name"
            | "browser.os.version"
            | "wafGroup"
            | "wafAction"
            | "geo.country.id"
            | "geo.province.id"
            | "geo.city.id"
            | "isp.id"
            | "geo.town.name"
            | "geo.town.id"
    )
}

fn template_variables(template: &str) -> impl Iterator<Item = &str> {
    let mut rest = template;
    std::iter::from_fn(move || {
        let start = rest.find("${")?;
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        rest = &after[end + 1..];
        Some(&after[..end])
    })
}

fn resolve_projection_key(
    template: &str,
    attrs: &BTreeMap<String, String>,
    dimensions: &ResolvedDimensions,
) -> String {
    crate::metrics::resolve_template_with(template, |var_name| {
        let resolved = crate::metrics::resolve_metric_variable_from_map(attrs, var_name);
        if !resolved.is_empty() && resolved != format!("${{{var_name}}}") {
            return resolved;
        }
        dimensions.resolve_variable(var_name, resolved)
    })
}

/// Per-period hour/minute bucketing and the exclusive end of the identity
/// window, using the event's own local wall clock (never flush or retry time).
fn period_window(
    period: i32,
    unit: &str,
    wall: chrono::DateTime<FixedOffset>,
) -> (Arc<str>, i64, i64) {
    let period = period.max(1) as u32;
    let offset = *wall.offset();
    let key = match unit.to_lowercase().as_str() {
        "month" => wall.format("%Y%m").to_string(),
        "week" => {
            let week = wall.iso_week();
            format!("{:04}{:02}", week.year(), week.week())
        }
        "day" => wall.format("%Y%m%d").to_string(),
        "hour" if period > 1 => {
            let hour = (wall.hour() / period) * period;
            format!("{}{:02}", wall.format("%Y%m%d"), hour)
        }
        "hour" => wall.format("%Y%m%d%H").to_string(),
        "minute" if period > 1 => {
            let minute = (wall.minute() / period) * period;
            format!("{}{:02}", wall.format("%Y%m%d%H"), minute)
        }
        "minute" => wall.format("%Y%m%d%H%M").to_string(),
        _ => wall.format("%Y%m%d%H%M").to_string(),
    };
    let start = match unit.to_lowercase().as_str() {
        "month" => {
            let date = wall.date_naive().with_day(1).unwrap_or(wall.date_naive());
            local_midnight(date, offset)
        }
        "week" => {
            let week = wall.iso_week();
            let monday = chrono::NaiveDate::from_isoywd_opt(week.year(), week.week(), Weekday::Mon)
                .unwrap_or(wall.date_naive());
            local_midnight(monday, offset)
        }
        "day" => local_midnight(wall.date_naive(), offset),
        "hour" => {
            let hour = if period > 1 {
                (wall.hour() / period) * period
            } else {
                wall.hour()
            };
            wall.with_minute(0)
                .and_then(|value| value.with_second(0))
                .and_then(|value| value.with_nanosecond(0))
                .and_then(|value| value.with_hour(hour))
                .unwrap_or(wall)
        }
        _ => {
            let minute = if period > 1 {
                (wall.minute() / period) * period
            } else {
                wall.minute()
            };
            wall.with_second(0)
                .and_then(|value| value.with_nanosecond(0))
                .and_then(|value| value.with_minute(minute))
                .unwrap_or(wall)
        }
    };
    let end = match unit.to_lowercase().as_str() {
        "month" => {
            let (year, month) = if wall.month() == 12 {
                (wall.year() + 1, 1)
            } else {
                (wall.year(), wall.month() + 1)
            };
            chrono::NaiveDate::from_ymd_opt(year, month, 1)
                .map(|date| local_midnight(date, offset))
                .unwrap_or_else(|| start + ChronoDuration::days(31))
        }
        "week" => start + ChronoDuration::days(7),
        "day" => start + ChronoDuration::days(1),
        "hour" => start + ChronoDuration::hours(period as i64),
        _ => start + ChronoDuration::minutes(period as i64),
    };
    (Arc::from(key), start.timestamp(), end.timestamp())
}

fn local_midnight(date: chrono::NaiveDate, offset: FixedOffset) -> chrono::DateTime<FixedOffset> {
    offset
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .single()
        .unwrap_or_else(|| offset.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()))
}

fn compile_plan(
    items: &[MetricItemConfig],
    generation: u64,
    fingerprint: u64,
) -> MetricCollectionPlan {
    let mut compiled = Vec::new();
    let mut http = CategoryPlan::default();
    let mut tcp = CategoryPlan::default();
    let mut udp = CategoryPlan::default();
    for item in items.iter().filter(|item| item.is_on) {
        let category = Arc::<str>::from(crate::metrics::normalize_metric_category(&item.category));
        let keys = item
            .keys
            .iter()
            .map(|key| Arc::<str>::from(key.as_str()))
            .collect::<Vec<_>>();
        let category_plan = match category.as_ref() {
            crate::metrics::METRIC_CATEGORY_TCP => &mut tcp,
            crate::metrics::METRIC_CATEGORY_UDP => &mut udp,
            _ => &mut http,
        };
        category_plan.items += 1;
        for template in &keys {
            for variable in template_variables(template) {
                if !is_analysis_variable(variable) {
                    category_plan.request_vars.insert(Arc::from(variable));
                }
            }
        }
        compiled.push(CompiledMetricItem {
            id: item.id,
            version: item.version,
            category,
            period: item.period,
            period_unit: Arc::from(item.period_unit.as_str()),
            value: MetricValueKind::compile(&item.value),
            keys: Arc::from(keys),
        });
    }
    let aggregator =
        (!compiled.is_empty()).then(|| Arc::new(MetricProjectionAggregator::new(generation)));
    MetricCollectionPlan {
        generation,
        fingerprint,
        items: Arc::from(compiled),
        aggregator,
        http,
        tcp,
        udp,
        inflight_events: AtomicUsize::new(0),
    }
}

static CURRENT: LazyLock<ArcSwap<Arc<MetricCollectionPlan>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Arc::new(MetricCollectionPlan::empty())));
static RETIRED: LazyLock<Mutex<Vec<Arc<MetricCollectionPlan>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
/// Serializes plan capture-with-lease against publish/retire. A request that
/// loads the current plan and then acquires its lease must not be able to lose
/// a race with the reporter retiring that generation in between.
static LIFECYCLE: RwLock<()> = RwLock::new(());

/// Latest immutable plan. Hot-path callers clone one `Arc` per event and hold
/// it until the event has been aggregated.
pub fn current_metric_plan() -> Arc<MetricCollectionPlan> {
    CURRENT.load().as_ref().clone()
}

/// Load the current plan and hold a request-scoped lease on it atomically with
/// respect to publish/retire. This is the capture point on the request path.
pub fn current_metric_plan_leased() -> MetricPlanLease {
    let _guard = LIFECYCLE.read();
    MetricPlanLease::new(current_metric_plan())
}

/// FNV-1a over the serialized item list. Only used to detect whether a config
/// sync actually changed the metric plan.
fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Publish a plan for this configuration snapshot. Re-publishing an identical
/// item list is a no-op, so ordinary config syncs do not churn generations.
pub fn publish_metric_items(items: &[MetricItemConfig]) -> Arc<MetricCollectionPlan> {
    let encoded = serde_json::to_vec(items).unwrap_or_default();
    let fingerprint = fingerprint_bytes(&encoded);
    let _guard = LIFECYCLE.write();
    let current = current_metric_plan();
    if current.fingerprint == fingerprint {
        return current;
    }
    let generation = NEXT_GENERATION.fetch_add(1, Ordering::AcqRel);
    let plan = Arc::new(compile_plan(items, generation, fingerprint));
    let previous = CURRENT.swap(Arc::new(Arc::clone(&plan)));
    if !previous.is_empty() {
        RETIRED.lock().push(previous.as_ref().clone());
    }
    plan
}

/// Plans that may still own accepted projections or in-flight events. The
/// reporter drains every one, then calls [`retain_drained`].
pub fn plans_for_drain() -> Vec<Arc<MetricCollectionPlan>> {
    let _guard = LIFECYCLE.read();
    let current = current_metric_plan();
    let retired = RETIRED.lock().clone();
    let mut plans = Vec::with_capacity(retired.len() + 1);
    if !current.is_empty() || !retired.is_empty() {
        plans.push(current);
    }
    plans.extend(retired);
    plans
}

/// Drop retired plans that no longer own data. The current plan is never
/// removed. A plan whose event queue has drained but whose aggregator still
/// holds data stays until the next reporter tick.
pub fn retain_drained() {
    let _guard = LIFECYCLE.write();
    let current = current_metric_plan();
    let mut retired = RETIRED.lock();
    retired.retain(|plan| !Arc::ptr_eq(plan, &current) && !plan.is_drained());
}

/// Live projection bytes across current and retired plans.
pub fn aggregator_bytes() -> u64 {
    super::projection::METRIC_PROJECTION_POOL.used()
}

pub fn dropped_projections() -> u64 {
    super::projection::dropped_projections()
}

#[cfg(test)]
mod tests {
    use super::{compile_plan, period_window, publish_metric_items};
    use crate::config_models::MetricItemConfig;
    use chrono::{TimeZone, Timelike};
    use std::collections::BTreeMap;

    fn item(id: i64, category: &str, period: i32, unit: &str, keys: &[&str]) -> MetricItemConfig {
        MetricItemConfig {
            id,
            name: String::new(),
            code: String::new(),
            category: category.into(),
            keys: keys.iter().map(|key| key.to_string()).collect(),
            value: serde_json::json!("${countRequest}"),
            period,
            period_unit: unit.into(),
            version: 1,
            is_on: true,
        }
    }

    #[test]
    fn value_kind_preserves_every_existing_expression_alias() {
        use super::MetricValueKind;
        use serde_json::json;

        let cases = [
            (json!("${bytesSent}"), MetricValueKind::BytesSent),
            (json!("${countTrafficOut}"), MetricValueKind::BytesSent),
            (json!("${countTrafficIn}"), MetricValueKind::BytesReceived),
            (json!("${countAttackRequest}"), MetricValueKind::AttackCount),
            (json!("${countRequest}"), MetricValueKind::Count),
            (json!("${countConnection}"), MetricValueKind::Count),
            // The previous builder used count for every non-alias expression,
            // including numeric literals and unknown templates.
            (json!(7), MetricValueKind::Count),
            (json!("${unknownTemplate}"), MetricValueKind::Count),
        ];
        for (value, expected) in cases {
            assert_eq!(MetricValueKind::compile(&value), expected, "{value}");
        }
    }

    #[test]
    fn period_zero_and_unknown_units_keep_the_old_minute_bucketing() {
        let wall = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 31, 12, 7, 30)
            .unwrap();
        assert_eq!(period_window(0, "minute", wall).0.as_ref(), "202610311207");
        assert_eq!(period_window(1, "unknown", wall).0.as_ref(), "202610311207");
        assert_eq!(period_window(5, "minute", wall).0.as_ref(), "202610311205");
        assert_eq!(period_window(2, "hour", wall).0.as_ref(), "2026103112");
    }

    #[test]
    fn disabled_items_compile_to_an_empty_plan() {
        let mut disabled = item(1, "http", 5, "minute", &["${country}"]);
        disabled.is_on = false;
        let plan = publish_metric_items(&[disabled]);
        assert!(plan.is_empty());
        assert!(plan.aggregator().is_none());
    }

    #[test]
    fn empty_plan_accepts_100k_events_without_any_projection_state() {
        let plan = compile_plan(&[], 1, 0);
        let attrs = BTreeMap::new();
        let empty = std::sync::Arc::<str>::from("");
        let dimensions = crate::metrics::projection::ResolvedDimensions {
            country: empty.clone(),
            country_id: 0,
            province: empty.clone(),
            province_id: 0,
            city: empty.clone(),
            city_id: 0,
            provider: empty.clone(),
            provider_id: 0,
            browser: empty.clone(),
            browser_version: empty.clone(),
            os: empty.clone(),
            os_version: empty.clone(),
            waf_group_id: 0,
            waf_action: empty,
        };
        for index in 0..100_000 {
            assert_eq!(
                plan.record_event(super::ProjectionEvent {
                    category: crate::metrics::METRIC_CATEGORY_HTTP,
                    server_id: 1,
                    event_time_millis: index,
                    attrs: &attrs,
                    dimensions: &dimensions,
                    bytes_sent: 1,
                    bytes_received: 0,
                    is_attack: false,
                }),
                0
            );
        }
        assert!(plan.is_drained());
        assert!(plan.aggregator().is_none());
    }

    #[test]
    fn request_lease_keeps_an_empty_generation_from_retiring() {
        let plan = std::sync::Arc::new(compile_plan(&[], 1, 0));
        assert!(plan.is_drained());
        let lease = super::MetricPlanLease::new(std::sync::Arc::clone(&plan));
        // A long request that captured this generation must keep it visible to
        // the reporter even when no event has been enqueued yet.
        assert!(!plan.is_drained());
        drop(lease);
        assert!(plan.is_drained());
    }

    #[test]
    fn plan_tracks_request_variables_only_for_enabled_items() {
        let plan = publish_metric_items(&[
            item(1, "http", 5, "minute", &["${country}"]),
            item(
                2,
                "http",
                5,
                "minute",
                &["${requestId}", "${header.X-Test}"],
            ),
        ]);
        let vars = plan
            .request_vars_for(crate::metrics::METRIC_CATEGORY_HTTP)
            .expect("requestId needs a request context");
        assert!(vars.contains("requestId"));
        assert!(vars.contains("header.X-Test"));
        assert!(!vars.contains("country"));
        assert!(
            plan.request_vars_for(crate::metrics::METRIC_CATEGORY_TCP)
                .is_none()
        );
    }

    #[test]
    fn event_time_drives_hour_and_minute_windows() {
        let wall = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 31, 12, 47, 30)
            .unwrap();
        let (key, start, end) = period_window(1, "hour", wall);
        assert_eq!(key.as_ref(), "2026103112");
        assert_eq!(
            start,
            wall.with_minute(0)
                .unwrap()
                .with_second(0)
                .unwrap()
                .timestamp()
        );
        assert_eq!(end - start, 3600);

        let (key, start, end) = period_window(5, "minute", wall);
        assert_eq!(key.as_ref(), "202610311245");
        assert_eq!(end - start, 300);

        let (key, _, _) = period_window(1, "day", wall);
        assert_eq!(key.as_ref(), "20261031");
        let (key, _, _) = period_window(1, "week", wall);
        assert_eq!(key.as_ref(), "202644");
        let (key, _, _) = period_window(1, "month", wall);
        assert_eq!(key.as_ref(), "202610");
    }

    #[test]
    fn event_from_an_old_minute_keeps_its_own_window() {
        let wall = chrono::FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 31, 12, 47, 30)
            .unwrap();
        let event_millis = wall.timestamp_millis();
        let later = wall + chrono::Duration::minutes(3);
        let (key, _, _) = period_window(1, "minute", later);
        assert_ne!(key.as_ref(), "202610311247");
        let (key, _, _) = period_window(1, "minute", wall);
        assert_eq!(key.as_ref(), "202610311247");
        assert!(event_millis > 0);
    }
}
