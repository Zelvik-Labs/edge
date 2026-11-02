//! Raw ABI for the owned C fixture; callers must preserve UMEM and SPSC lifetimes.
use std::ffi::{c_char, c_void};

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct NativeStats {
    pub received: u64,
    pub submitted: u64,
    pub completed: u64,
    pub fill_kicks: u64,
}

unsafe extern "C" {
    pub fn edge_xsk_open(rx: *const c_char, peer: *const c_char, error: *mut i32) -> *mut c_void;
    pub fn edge_xsk_fd(context: *mut c_void) -> i32;
    pub fn edge_xsk_peer_fd(context: *mut c_void) -> i32;
    pub fn edge_xsk_peer_send(context: *mut c_void, sequence: u64) -> i32;
    pub fn edge_xsk_peer_receive(context: *mut c_void, sequence: u64) -> i32;
    pub fn edge_xsk_rx_pending(context: *mut c_void) -> i32;
    pub fn edge_xsk_receive_only(context: *mut c_void, sequence: u64) -> i32;
    pub fn edge_xsk_echo(context: *mut c_void, sequence: u64) -> i32;
    pub fn edge_xsk_reclaim(context: *mut c_void) -> i32;
    pub fn edge_xsk_stats(context: *mut c_void, output: *mut NativeStats);
    pub fn edge_xsk_close(context: *mut c_void) -> i32;
    pub fn edge_xsk_failure_stage() -> *const c_char;
}
