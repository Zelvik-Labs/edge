//! Kernel I/O primitives. Admission, routing and metrics remain with callers.

#[cfg(target_os = "linux")]
pub mod splice;

#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub mod uring;
