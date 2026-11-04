//! Measure real callback construction/drop, without polling business logic.
use std::time::Instant;

pub fn blocks(mut operation: impl FnMut()) -> Vec<serde_json::Value> {
    for _ in 0..10_000 {
        operation();
    }
    let mut rows = Vec::with_capacity(20);
    for block in 0..20 {
        let iterations = 100_000;
        let start = Instant::now();
        for _ in 0..iterations {
            operation();
        }
        let nanoseconds = start.elapsed().as_nanos();
        rows.push(serde_json::json!({
            "block": block,
            "iterations": iterations,
            "nanoseconds": nanoseconds,
        }));
    }
    rows
}
