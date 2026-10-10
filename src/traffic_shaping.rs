//! Aggregate byte scheduling shared by every connection on this node.
//!
//! Two scopes share one scheduler:
//! - `ShapingScope::Site`: a site's own `trafficLimit` (one bucket per site).
//! - `ShapingScope::Plan`: a plan instance (subscription). Every site bound to
//!   the same instance draws from one bucket, so the plan limit is shared
//!   across the subscription instead of being granted per site.
use dashmap::DashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
static SCHEDULES: LazyLock<DashMap<(i64, bool), Instant>> = LazyLock::new(DashMap::new);
const MAX_SCHEDULES: usize = 200_000;

/// Scheduler key. Site ids from the control plane are masked to 52 bits and
/// stay non-negative, so negating the plan instance id keeps the two scopes in
/// disjoint key spaces (a site and a plan can never share a bucket).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapingScope {
    Site(i64),
    Plan(i64),
}

impl ShapingScope {
    fn key(self) -> i64 {
        match self {
            ShapingScope::Site(id) => id,
            ShapingScope::Plan(id) => -id,
        }
    }
}

pub fn reserve(
    server: i64,
    upload: bool,
    mbps: f64,
    bytes: usize,
) -> Result<Duration, &'static str> {
    reserve_scoped(ShapingScope::Site(server), upload, mbps, bytes)
}

pub fn reserve_scoped(
    scope: ShapingScope,
    upload: bool,
    mbps: f64,
    bytes: usize,
) -> Result<Duration, &'static str> {
    if mbps == 0.0 || bytes == 0 {
        return Ok(Duration::ZERO);
    }
    if !mbps.is_finite() || mbps < 0.0 {
        return Err("invalid site bandwidth limit");
    }
    let key = scope.key();
    let now = Instant::now();
    let duration = Duration::try_from_secs_f64(bytes as f64 * 8.0 / (mbps * 1_000_000.0))
        .map_err(|_| "invalid bandwidth scheduling duration")?;
    if !SCHEDULES.contains_key(&(key, upload)) && SCHEDULES.len() >= MAX_SCHEDULES {
        SCHEDULES.retain(|_, deadline| *deadline > now);
        if SCHEDULES.len() >= MAX_SCHEDULES {
            return Err("site bandwidth scheduler capacity exceeded");
        }
    }
    let mut deadline = SCHEDULES.entry((key, upload)).or_insert(now);
    let next = (*deadline)
        .max(now)
        .checked_add(duration)
        .ok_or("bandwidth scheduling overflow")?;
    *deadline = next;
    Ok(next.saturating_duration_since(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aggregates_connections_and_separates_directions() {
        let a = reserve(-4321, false, 8.0, 1_000_000).unwrap();
        let b = reserve(-4321, false, 8.0, 1_000_000).unwrap();
        let upload = reserve(-4321, true, 8.0, 1_000_000).unwrap();
        assert!(a >= Duration::from_millis(999));
        assert!(b >= Duration::from_millis(1990));
        assert!(upload < Duration::from_millis(1100));
    }
    #[test]
    fn invalid_rates_are_visible_errors() {
        assert!(reserve(1, false, f64::NAN, 1).is_err());
        assert_eq!(reserve(1, false, 0.0, 123).unwrap(), Duration::ZERO);
    }
    #[test]
    fn plan_scope_is_shared_across_sites() {
        // 同一套餐实例的两个站点共用同一桶：第二次预留要排队。
        let first = reserve_scoped(ShapingScope::Plan(900_001), false, 8.0, 1_000_000).unwrap();
        let second = reserve_scoped(ShapingScope::Plan(900_001), false, 8.0, 1_000_000).unwrap();
        assert!(first >= Duration::from_millis(999));
        assert!(second >= Duration::from_millis(1990));
        // 另一个套餐实例互不影响。
        let other = reserve_scoped(ShapingScope::Plan(900_002), false, 8.0, 1_000_000).unwrap();
        assert!(other < Duration::from_millis(1100));
    }
    #[test]
    fn site_and_plan_scopes_never_share_a_bucket() {
        // 站点 id 与套餐实例 id 即使数值相同也必须落在不同桶。
        let site = reserve_scoped(ShapingScope::Site(900_003), false, 8.0, 1_000_000).unwrap();
        let plan = reserve_scoped(ShapingScope::Plan(900_003), false, 8.0, 1_000_000).unwrap();
        assert!(site >= Duration::from_millis(999));
        assert!(plan < Duration::from_millis(1100));
    }
}
