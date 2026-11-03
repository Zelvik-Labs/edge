use super::cooperate_after_round;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test(flavor = "current_thread")]
async fn ready_idle_rounds_allow_other_task_progress() {
    for cooperative in [false, true] {
        let progressed = Arc::new(AtomicBool::new(false));
        let other = progressed.clone();
        let task = tokio::spawn(async move { other.store(true, Ordering::SeqCst) });
        // Finite even when the candidate is broken: no timeout depends on a
        // starved current-thread timer getting polled.
        for _ in 0..4096 {
            cooperate_after_round(true, cooperative).await;
        }
        assert!(progressed.load(Ordering::SeqCst));
        task.await.expect("progress task completed");
    }
}

#[test]
fn busy_rounds_remain_pending_on_the_first_poll() {
    for cooperative in [false, true] {
        let mut future = Box::pin(cooperate_after_round(false, cooperative));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(future.as_mut(), &mut context).is_pending());
    }
}
