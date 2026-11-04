//! Compile the actual queue helper independently so scheduling mutation
//! controls do not rebuild the entire node or change its tested implementation.
#[path = "../../../../src/xdp/af_xdp/cooperation.rs"]
mod cooperation;

pub use cooperation::cooperate_after_round;
