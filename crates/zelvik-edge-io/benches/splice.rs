//! Kernel relay component benchmark. Client, echo origin and relay share one
//! process; these are not full Edge/SNI capacity measurements.

#[cfg(target_os = "linux")]
mod support;

#[cfg(target_os = "linux")]
use criterion::{BenchmarkId, Criterion};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
fn main() {
    let selected = match std::env::var("EDGE_SPLICE_BENCH_ENGINE") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::from("all"),
        Err(error) => panic!("invalid EDGE_SPLICE_BENCH_ENGINE: {error}"),
    };
    let engines: &[_] = match selected.as_str() {
        "blocking" => &[support::Engine::Blocking],
        "readiness" => &[support::Engine::Readiness],
        "all" => &[support::Engine::Blocking, support::Engine::Readiness],
        _ => panic!("EDGE_SPLICE_BENCH_ENGINE must be blocking, readiness or all"),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut c = Criterion::default().configure_from_args();
    let mut group = c.benchmark_group("kernel_relay_roundtrip");
    group
        .sample_size(20)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    for size in [1024, 64 * 1024, 1024 * 1024] {
        for &engine in engines {
            let connection = runtime.block_on(support::Connection::new(engine, size));
            group.bench_with_input(BenchmarkId::new(engine.name(), size), &size, |b, _| {
                b.to_async(&runtime).iter(|| connection.exchange());
            });
            runtime.block_on(connection.finish());
        }
    }
    group.finish();
    c.final_summary();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("kernel splice benchmark requires Linux");
    std::process::exit(2);
}
