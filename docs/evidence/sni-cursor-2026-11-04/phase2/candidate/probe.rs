//! Standalone probe copied beside an exact production parser by the CI builder.
mod parser;

use parser::{ClientHelloParse, parse_tls_client_hello_sni};
use std::hint::black_box;
use std::time::Instant;

#[cfg(feature = "count-allocations")]
mod allocations {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    pub struct Counter;
    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static COUNT: AtomicU64 = AtomicU64::new(0);
    static REALLOC: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static LARGEST: AtomicU64 = AtomicU64::new(0);

    fn record(size: usize, realloc: bool) {
        if ACTIVE.load(Ordering::Relaxed) {
            if realloc {
                REALLOC.fetch_add(1, Ordering::Relaxed);
            } else {
                COUNT.fetch_add(1, Ordering::Relaxed);
            }
            BYTES.fetch_add(size as u64, Ordering::Relaxed);
            LARGEST.fetch_max(size as u64, Ordering::Relaxed);
        }
    }

    // SAFETY: every operation delegates the exact pointer/layout contract to
    // System. The counter only touches atomics and cannot allocate recursively.
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            record(layout.size(), false);
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            record(layout.size(), false);
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            record(size, true);
            unsafe { System.realloc(pointer, layout, size) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
    }

    pub fn start() {
        COUNT.store(0, Ordering::Relaxed);
        REALLOC.store(0, Ordering::Relaxed);
        BYTES.store(0, Ordering::Relaxed);
        LARGEST.store(0, Ordering::Relaxed);
        ACTIVE.store(true, Ordering::Relaxed);
    }

    pub fn finish() -> [u64; 4] {
        ACTIVE.store(false, Ordering::Relaxed);
        [
            COUNT.load(Ordering::Relaxed),
            REALLOC.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed),
            LARGEST.load(Ordering::Relaxed),
        ]
    }
}

#[cfg(feature = "count-allocations")]
#[global_allocator]
static ALLOCATOR: allocations::Counter = allocations::Counter;

fn record(payload: &[u8]) -> Vec<u8> {
    let mut wire = vec![22, 3, 3];
    wire.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
    wire.extend_from_slice(payload);
    wire
}

fn hello(padding: usize) -> Vec<u8> {
    let host = b"Example.COM";
    let mut extensions = vec![0, 0];
    extensions.extend_from_slice(&((5 + host.len()) as u16).to_be_bytes());
    extensions.extend_from_slice(&((3 + host.len()) as u16).to_be_bytes());
    extensions.push(0);
    extensions.extend_from_slice(&(host.len() as u16).to_be_bytes());
    extensions.extend_from_slice(host);
    if padding > 0 {
        extensions.extend_from_slice(&21u16.to_be_bytes());
        extensions.extend_from_slice(&u16::try_from(padding).unwrap().to_be_bytes());
        extensions.resize(extensions.len() + padding, 0);
    }
    let mut body = vec![3, 3];
    body.extend_from_slice(&[0; 32]);
    body.extend_from_slice(&[0, 0, 2, 0x13, 1, 1, 0]);
    body.extend_from_slice(&u16::try_from(extensions.len()).unwrap().to_be_bytes());
    body.extend_from_slice(&extensions);
    let mut handshake = vec![
        1,
        (body.len() >> 16) as u8,
        (body.len() >> 8) as u8,
        body.len() as u8,
    ];
    handshake.extend_from_slice(&body);
    handshake
}

fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let small = hello(0);
    let large = hello(8192);
    let mut fragmented = Vec::new();
    for chunk in large.chunks(1024) {
        fragmented.extend_from_slice(&record(chunk));
    }
    let mut trailing = record(&small[..12]);
    let mut tail = small[12..].to_vec();
    tail.extend_from_slice(&[0xa5; 8192]);
    trailing.extend_from_slice(&record(&tail));
    vec![
        ("single", record(&small)),
        ("fragmented", fragmented),
        ("partial", record(&large[..2048])),
        ("oversized", record(&[1, 0xff, 0xff, 0xff])),
        ("trailing", trailing),
    ]
}

fn corpus(cases: &[(&str, Vec<u8>)]) {
    for (name, wire) in cases {
        for end in 0..=wire.len() {
            println!(
                "{name}\t{end}\t{:?}",
                parse_tls_client_hello_sni(&wire[..end])
            );
        }
    }
    let mut state = 0x4218_8a71u32;
    for index in 0..4096 {
        let mut wire = cases[index % cases.len()].1.clone();
        for _ in 0..3 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let offset = state as usize % wire.len();
            wire[offset] ^= (state >> 24) as u8;
        }
        println!("mutation\t{index}\t{:?}", parse_tls_client_hello_sni(&wire));
    }
    // Exercise field boundaries and malformed lengths on every possible
    // supported split, not mostly padding in the large fixture.
    let small = hello(0);
    for split in 4..small.len() {
        let mut wire = record(&small[..split]);
        wire.extend_from_slice(&record(&[]));
        wire.extend_from_slice(&record(&small[split..]));
        println!("split\t{split}\t{:?}", parse_tls_client_hello_sni(&wire));
        for offset in 0..wire.len() {
            for bit in 0..8 {
                wire[offset] ^= 1 << bit;
                println!(
                    "split-mutation\t{split}\t{offset}\t{bit}\t{:?}",
                    parse_tls_client_hello_sni(&wire)
                );
                wire[offset] ^= 1 << bit;
            }
        }
    }
}

fn timing(cases: &[(&str, Vec<u8>)]) {
    println!("case\tblock\titerations\tnanoseconds");
    for (name, wire) in cases {
        let start = Instant::now();
        for _ in 0..4096 {
            black_box(parse_tls_client_hello_sni(black_box(wire)));
        }
        let per_call = (start.elapsed().as_nanos() / 4096).max(1);
        let iterations = (10_000_000 / per_call).clamp(1024, 1_000_000) as usize;
        for block in 0..20 {
            let start = Instant::now();
            for _ in 0..iterations {
                black_box(parse_tls_client_hello_sni(black_box(wire)));
            }
            println!(
                "{name}\t{block}\t{iterations}\t{}",
                start.elapsed().as_nanos()
            );
        }
    }
}

fn main() {
    let cases = fixtures();
    for (name, wire) in &cases {
        let result = parse_tls_client_hello_sni(wire);
        if matches!(*name, "partial" | "oversized") {
            assert!(matches!(result, ClientHelloParse::NeedMore));
        } else {
            assert!(matches!(result, ClientHelloParse::Found(ref host) if host == "example.com"));
        }
    }
    match std::env::args().nth(1).as_deref() {
        Some("--corpus") => corpus(&cases),
        Some("--timing") => timing(&cases),
        #[cfg(feature = "count-allocations")]
        Some(mode @ ("--allocations" | "--assert-bounded")) => {
            println!("case\tallocations\treallocations\trequested_bytes\tmax_allocation_bytes");
            for (name, wire) in &cases {
                allocations::start();
                black_box(parse_tls_client_hello_sni(black_box(wire)));
                let counts = allocations::finish();
                println!(
                    "{name}\t{}\t{}\t{}\t{}",
                    counts[0], counts[1], counts[2], counts[3]
                );
                if mode == "--assert-bounded" && *name == "oversized" {
                    assert_eq!(
                        counts, [0; 4],
                        "incomplete untrusted length must not allocate"
                    );
                }
            }
        }
        _ => panic!("choose --timing, --corpus or an allocation-probe mode"),
    }
}
