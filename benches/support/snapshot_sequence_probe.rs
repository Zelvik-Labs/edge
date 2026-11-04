use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

mod sequence {
    use std::sync::atomic::{AtomicU64, Ordering, fence};
    include!("snapshot_sequence_candidate.rs");
}

struct Snapshot {
    timestamp: AtomicU64,
    fields: [AtomicU64; 20],
    sequence: sequence::SnapshotSequence,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            timestamp: AtomicU64::new(77),
            fields: std::array::from_fn(|_| AtomicU64::new(1)),
            sequence: sequence::SnapshotSequence::new(),
        }
    }

    #[inline]
    fn read(&self, candidate: bool, fields: usize) -> Option<u64> {
        let version = if candidate {
            Some(self.sequence.begin_read()?)
        } else {
            None
        };
        let timestamp = self.timestamp.load(Ordering::Acquire);
        if timestamp == 0 {
            return None;
        }
        let mut sum = 0;
        for field in &self.fields[..fields] {
            sum += field.load(Ordering::Relaxed);
        }
        let valid = if let Some(version) = version {
            self.sequence.read_valid(version)
        } else {
            self.timestamp.load(Ordering::Acquire) == timestamp
        };
        valid.then_some(sum)
    }

    fn write(&self, candidate: bool, value: u64) {
        let version = candidate.then(|| self.sequence.begin_write());
        self.timestamp.store(0, Ordering::Relaxed);
        for field in &self.fields {
            field.store(value, Ordering::Relaxed);
        }
        self.timestamp.store(77, Ordering::Relaxed);
        if let Some(version) = version {
            self.sequence.finish_write(version);
        }
    }
}

fn controls() {
    let state = Snapshot::new();
    let before = state.timestamp.load(Ordering::Acquire);
    let old = state.fields[0].load(Ordering::Relaxed);
    state.write(false, 2);
    let new = state.fields[1].load(Ordering::Relaxed);
    assert_ne!(old, new);
    assert_eq!(state.timestamp.load(Ordering::Acquire), before);
    println!("timestamp_same_millisecond_accepts_mixed\ttrue");
    let before = state.sequence.begin_read().unwrap();
    let old = state.fields[0].load(Ordering::Relaxed);
    state.write(true, 3);
    let new = state.fields[1].load(Ordering::Relaxed);
    assert_ne!(old, new);
    assert!(!state.sequence.read_valid(before));
    println!("version_same_millisecond_rejects_mixed\ttrue");
    let interrupted = state.sequence.begin_write();
    assert!(state.sequence.begin_read().is_none());
    let recovered = state.sequence.begin_write();
    assert_eq!(interrupted, recovered);
    state.sequence.finish_write(recovered);
    assert_eq!(state.sequence.begin_read(), Some(recovered));
    assert_eq!(state.read(true, 20), Some(60));
    println!("interrupted_writer_recovers_without_partial_commit\ttrue");
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["verify"] {
        controls();
        return;
    }
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], "timing");
    let candidate = match args[1].as_str() {
        "baseline" => false,
        "candidate" => true,
        _ => panic!("unknown variant"),
    };
    println!("case\tblock\titerations\tnanoseconds\tchecksum");
    for (case, fields, cadence) in [
        ("read-2", 2, 0),
        ("read-20", 20, 0),
        ("publish-every-1024", 20, 1024),
        ("publish-every-16", 20, 16),
    ] {
        let state = Snapshot::new();
        for block in 0..20 {
            let iterations = 1_000_000_u64;
            let mut checksum = 0;
            let start = Instant::now();
            for i in 0..iterations {
                if cadence != 0 && i % cadence == 0 {
                    state.write(candidate, 1);
                }
                checksum += black_box(black_box(&state).read(candidate, fields).unwrap());
            }
            let ns = start.elapsed().as_nanos();
            assert_eq!(checksum, iterations * fields as u64);
            println!("{case}\t{block}\t{iterations}\t{ns}\t{checksum}");
        }
    }
}
