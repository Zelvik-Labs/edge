//! Aggregate per-site byte scheduling shared by every connection on this node.
use dashmap::DashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
static SCHEDULES: LazyLock<DashMap<(i64, bool), Instant>> = LazyLock::new(DashMap::new);
const MAX_SCHEDULES: usize = 200_000;

pub fn reserve(
    server: i64,
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
    let now = Instant::now();
    let duration = Duration::try_from_secs_f64(bytes as f64 * 8.0 / (mbps * 1_000_000.0))
        .map_err(|_| "invalid bandwidth scheduling duration")?;
    if !SCHEDULES.contains_key(&(server, upload)) && SCHEDULES.len() >= MAX_SCHEDULES {
        SCHEDULES.retain(|_, deadline| *deadline > now);
        if SCHEDULES.len() >= MAX_SCHEDULES {
            return Err("site bandwidth scheduler capacity exceeded");
        }
    }
    let mut deadline = SCHEDULES.entry((server, upload)).or_insert(now);
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
}
