type Counter = std::sync::Arc<AtomicU64>;
fn new_counter() -> Counter { std::sync::Arc::new(AtomicU64::new(0)) }
fn counter_arg(counter: &Counter) -> Counter { counter.clone() }
