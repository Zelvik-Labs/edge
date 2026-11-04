// Reuse the I/O crate's public-API contract tests in the root integration lane.
#[cfg(target_os = "linux")]
#[path = "../crates/zelvik-edge-io/tests/splice.rs"]
mod splice;
