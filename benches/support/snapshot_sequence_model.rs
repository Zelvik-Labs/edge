use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;
use std::sync::atomic::{AtomicU64 as ExecutionCount, Ordering as CountOrdering};

mod sequence {
    use loom::sync::atomic::{AtomicU64, Ordering, fence};
    include!("snapshot_sequence_candidate.rs");
}

#[derive(Clone, Copy)]
enum Protocol {
    Timestamp,
    Unfenced,
    Versioned,
}

fn check(protocol: Protocol, writers: usize) -> u64 {
    let executions = std::sync::Arc::new(ExecutionCount::new(0));
    let count = executions.clone();
    let mut builder = loom::model::Builder::new();
    // Exhaust all single-writer schedules. The three-thread model explicitly
    // checks up to two preemptions; it does not claim unbounded verification.
    builder.preemption_bound = (writers > 1).then_some(2);
    builder.check(move || {
        count.fetch_add(1, CountOrdering::Relaxed);
        let fields = Arc::new([AtomicU64::new(1), AtomicU64::new(1)]);
        let timestamp = Arc::new(AtomicU64::new(77));
        let version = Arc::new(AtomicU64::new(0));
        let sequence = Arc::new(sequence::SnapshotSequence::new());
        let lock = Arc::new(Mutex::new(()));
        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let fields = fields.clone();
                let timestamp = timestamp.clone();
                let version = version.clone();
                let sequence = sequence.clone();
                let lock = lock.clone();
                thread::spawn(move || {
                    let _guard = lock.lock().unwrap();
                    let stamp = match protocol {
                        Protocol::Timestamp => {
                            timestamp.store(0, Ordering::Relaxed);
                            77
                        }
                        Protocol::Unfenced => {
                            let old = version.load(Ordering::Relaxed);
                            version.store(old + 1, Ordering::Relaxed);
                            old + 2
                        }
                        Protocol::Versioned => sequence.begin_write(),
                    };
                    fields[0].store(writer as u64 + 2, Ordering::Relaxed);
                    fields[1].store(writer as u64 + 2, Ordering::Relaxed);
                    match protocol {
                        Protocol::Timestamp => timestamp.store(stamp, Ordering::Relaxed),
                        Protocol::Unfenced => version.store(stamp, Ordering::Release),
                        Protocol::Versioned => sequence.finish_write(stamp),
                    }
                })
            })
            .collect();
        let before = match protocol {
            Protocol::Timestamp => Some(timestamp.load(Ordering::Acquire)).filter(|s| *s != 0),
            Protocol::Unfenced => Some(version.load(Ordering::Acquire)).filter(|s| s & 1 == 0),
            Protocol::Versioned => sequence.begin_read(),
        };
        if let Some(before) = before {
            let a = fields[0].load(Ordering::Relaxed);
            let b = fields[1].load(Ordering::Relaxed);
            let valid = match protocol {
                Protocol::Timestamp => timestamp.load(Ordering::Acquire) == before,
                Protocol::Unfenced => version.load(Ordering::Relaxed) == before,
                Protocol::Versioned => sequence.read_valid(before),
            };
            if valid {
                assert_eq!(a, b, "accepted torn snapshot");
            }
        }
        for handle in handles {
            handle.join().unwrap();
        }
    });
    executions.load(CountOrdering::Relaxed)
}

fn main() {
    // Both negative controls must fail the same consistency invariant.
    for (name, protocol) in [
        ("timestamp-ABA", Protocol::Timestamp),
        ("version-without-fences", Protocol::Unfenced),
    ] {
        let failure = std::panic::catch_unwind(|| check(protocol, 1));
        assert!(failure.is_err(), "negative control did not expose {name}");
        println!("{{\"case\":\"{name}\",\"expected_counterexample\":true}}");
    }
    for writers in [1, 2] {
        let schedules = check(Protocol::Versioned, writers);
        assert!(schedules > 0);
        println!(
            "{{\"case\":\"versioned\",\"writers\":{writers},\"schedules\":{schedules},\"preemption_bound\":{}}}",
            if writers == 1 { "null" } else { "2" }
        );
    }
}
