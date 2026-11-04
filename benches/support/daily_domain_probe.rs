//! Exact map component; governor capacity and drop sink are explicit fixtures.
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

mod baseline;
mod candidate;
include!("allocator.rs");

static CAPACITY: AtomicUsize = AtomicUsize::new(4096);
static DROPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "count-allocations")]
static CAPACITY_CALLS: AtomicUsize = AtomicUsize::new(0);

mod memory_governor {
    pub struct Governor;
    pub static MEMORY_GOVERNOR: Governor = Governor;

    impl Governor {
        pub fn metrics_cardinality_capacity(&self) -> usize {
            #[cfg(feature = "count-allocations")]
            super::CAPACITY_CALLS.fetch_add(1, super::Ordering::Relaxed);
            super::CAPACITY.load(super::Ordering::Relaxed)
        }
    }
}

mod pipeline_metrics {
    pub fn note_cardinality_drop(name: &str, live: usize, capacity: usize) {
        assert_eq!(name, "daily_domain_tracker");
        assert!(live >= capacity);
        super::DROPS.fetch_add(1, super::Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Row(i64, i64, String, [i64; 6]);

trait Tracker: Default + Send + Sync + 'static {
    fn add(&self, server: i64, day: i64, domain: Arc<str>);
    fn legacy(&self, domain: &str);
    fn bytes(&self) -> u64;
    fn flush(&self, day: i64) -> Vec<Row>;
}

macro_rules! tracker {
    ($module:ident, $method:ident, $domain:ident, $argument:expr) => {
        impl Tracker for $module::DailyDomainTracker {
            fn add(&self, server: i64, day: i64, $domain: Arc<str>) {
                self.$method(server, day, $argument, 13, 5, 2, 1, 1, 3);
            }
            fn legacy(&self, domain: &str) {
                self.record(1, 100, domain, 13, 5, 2, 1, 1, 3);
            }
            fn bytes(&self) -> u64 {
                self.approximate_bytes()
            }
            fn flush(&self, day: i64) -> Vec<Row> {
                let mut rows: Vec<_> = self
                    .flush_older_than(day)
                    .into_iter()
                    .map(|(s, d, name, v)| {
                        Row(
                            s,
                            d,
                            name,
                            [
                                v.bytes,
                                v.cached_bytes,
                                v.count_requests,
                                v.count_cached_requests,
                                v.count_attack_requests,
                                v.attack_bytes,
                            ],
                        )
                    })
                    .collect();
                rows.sort();
                rows
            }
        }
    };
}
tracker!(baseline, record, domain, domain.as_ref());
tracker!(candidate, record_shared, domain, domain);

const CASES: [&str; 8] = [
    "hot-1",
    "hot-16",
    "hot-1024",
    "at-cap",
    "reject-new",
    "insert-new",
    "invalid",
    "legacy-hot",
];

fn fixture<T: Tracker>(case: &str, iterations: usize) -> (T, Vec<Arc<str>>) {
    let count = match case {
        "hot-16" | "at-cap" => 16,
        "hot-1024" => 1024,
        "insert-new" => iterations,
        _ => 1,
    };
    let domains: Vec<Arc<str>> = (0..count)
        .map(|i| Arc::from(format!("domain-{i}.example")))
        .collect();
    CAPACITY.store(
        if case == "at-cap" {
            16
        } else if case == "reject-new" {
            0
        } else {
            100_000
        },
        Ordering::Relaxed,
    );
    let tracker = T::default();
    if !matches!(case, "reject-new" | "insert-new" | "invalid") {
        for domain in &domains {
            tracker.add(1, 100, Arc::clone(domain));
        }
    }
    (tracker, domains)
}

fn work<T: Tracker>(tracker: &T, domains: &[Arc<str>], case: &str, iterations: usize) {
    for i in 0..iterations {
        let domain = black_box(&domains[i % domains.len()]);
        if case == "legacy-hot" {
            tracker.legacy(domain);
        } else {
            tracker.add(
                if case == "invalid" { 0 } else { 1 },
                100,
                Arc::clone(domain),
            );
        }
    }
}

fn timing<T: Tracker>() {
    println!("case\tblock\titerations\tnanoseconds");
    for case in CASES {
        for block in 0..20 {
            let iterations = if case == "insert-new" { 4096 } else { 20_000 };
            let (tracker, domains) = fixture::<T>(case, iterations);
            work(
                &tracker,
                &domains,
                case,
                if case == "insert-new" { 0 } else { 1000 },
            );
            let start = Instant::now();
            work(&tracker, &domains, case, iterations);
            let nanoseconds = start.elapsed().as_nanos();
            black_box(tracker.bytes());
            black_box(tracker.flush(i64::MAX));
            println!("{case}\t{block}\t{iterations}\t{nanoseconds}");
        }
    }
}

#[cfg(feature = "count-allocations")]
fn allocation_counts<T: Tracker>() {
    println!("case\titerations\tallocations\treallocations\tbytes\tlargest\tcapacity_calls");
    for case in CASES {
        let iterations = 1000;
        let (tracker, domains) = fixture::<T>(case, iterations);
        CAPACITY_CALLS.store(0, Ordering::Relaxed);
        allocations::start();
        work(&tracker, &domains, case, iterations);
        let [allocs, reallocs, bytes, largest] = allocations::finish();
        let calls = CAPACITY_CALLS.load(Ordering::Relaxed);
        black_box(tracker.flush(i64::MAX));
        println!("{case}\t{iterations}\t{allocs}\t{reallocs}\t{bytes}\t{largest}\t{calls}");
    }
}

fn concurrent<T: Tracker>() -> [i64; 6] {
    CAPACITY.store(100_000, Ordering::Relaxed);
    let tracker = Arc::new(T::default());
    let mut total = [0; 6];
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let tracker = Arc::clone(&tracker);
            scope.spawn(move || {
                let domain = Arc::from("concurrent.example");
                for _ in 0..2000 {
                    tracker.add(1, 100, Arc::clone(&domain));
                }
            });
        }
        for _ in 0..1000 {
            for row in tracker.flush(101) {
                for (sum, value) in total.iter_mut().zip(row.3) {
                    *sum += value;
                }
            }
            std::thread::yield_now();
        }
    });
    for row in tracker.flush(101) {
        for (sum, value) in total.iter_mut().zip(row.3) {
            *sum += value;
        }
    }
    total
}

fn verify() {
    println!("case\tstatus\tbaseline\tcandidate");
    let domains: Vec<Arc<str>> = (0..32)
        .map(|i| Arc::from(format!("domain-{i}.example")))
        .chain([
            Arc::from(""),
            Arc::from("x".repeat(127)),
            Arc::from("x".repeat(128)),
        ])
        .collect();
    let mut index = 0;
    for capacity in [0, 1, 2, 16, 64] {
        CAPACITY.store(capacity, Ordering::Relaxed);
        let old = baseline::DailyDomainTracker::new();
        let new = candidate::DailyDomainTracker::new();
        let mut seed = 20261104_u64;
        for step in 0..4096 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let domain = &domains[(seed >> 32) as usize % domains.len()];
            let server = ((seed >> 16) % 5) as i64 - 1;
            let day = 100 + (seed % 2) as i64;
            DROPS.store(0, Ordering::Relaxed);
            old.add(server, day, Arc::clone(domain));
            let drops = DROPS.swap(0, Ordering::Relaxed);
            new.add(server, day, Arc::clone(domain));
            assert_eq!(drops, DROPS.load(Ordering::Relaxed));
            assert_eq!(old.bytes(), new.bytes());
            let a = format!("{}:{drops}", old.bytes());
            println!("{index}\tPASS\t{a}\t{a}");
            index += 1;
            if step % 257 == 0 {
                let a = old.flush(101);
                let b = new.flush(101);
                assert_eq!(a, b);
                println!("{index}\tPASS\t{a:?}\t{b:?}");
                index += 1;
            }
        }
        let a = old.flush(102);
        let b = new.flush(102);
        assert_eq!(a, b);
        println!("{index}\tPASS\t{a:?}\t{b:?}");
        index += 1;
    }
    let a = concurrent::<baseline::DailyDomainTracker>();
    let b = concurrent::<candidate::DailyDomainTracker>();
    assert_eq!(a, [104_000, 40_000, 16_000, 8_000, 8_000, 24_000]);
    assert_eq!(a, b);
    println!("{index}\tPASS\t{a:?}\t{b:?}");
}

fn run<T: Tracker>(mode: &str) {
    match mode {
        "timing" => timing::<T>(),
        #[cfg(feature = "count-allocations")]
        "allocations" => allocation_counts::<T>(),
        _ => panic!("unsupported measurement mode"),
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "verify" {
        verify();
        return;
    }
    match args[2].as_str() {
        "baseline" => run::<baseline::DailyDomainTracker>(&args[1]),
        "candidate" => run::<candidate::DailyDomainTracker>(&args[1]),
        _ => panic!("unsupported variant"),
    }
}
