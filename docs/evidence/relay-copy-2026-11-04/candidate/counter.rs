type Counter = AtomicU64;
fn new_counter() -> Counter { AtomicU64::new(0) }
fn counter_arg(counter: &Counter) -> &Counter { counter }
