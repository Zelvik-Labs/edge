/// Busy rounds always yield. An idle select may complete immediately, so its
/// experimental path still spends a cooperative token to bound ready-event loops.
pub async fn cooperate_after_round(idle_waited: bool, cooperative_idle: bool) {
    if idle_waited && cooperative_idle {
        tokio::task::consume_budget().await;
    } else {
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
#[path = "cooperation_tests.rs"]
mod tests;
