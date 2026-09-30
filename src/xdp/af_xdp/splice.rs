//! Layer-4 fast path: promote an established smoltcp relay pair to a
//! pure eBPF splice (XDP_TCP_SPLICE), after which both wire directions
//! are rewritten and redirected in-kernel and the smoltcp legs only
//! see control traffic.
//!
//! Promotion contract (mirrored by `AfXdpTcpReactor::splice_begin` /
//! `splice_poll` / `splice_commit`): phase 1 arms *flush mode* on each
//! leg — the leg's current `rcv_nxt` becomes the splice boundary.
//! Positions below it keep draining through the app pipeline onto the
//! sibling wire as the pre-splice tail; positions at or beyond it park
//! unacked. Once both legs report their pipeline flushed to the
//! boundary (`splice_poll`), the map entries go live with
//!
//!   seq_delta = peer_snd_nxt - boundary
//!   ack_delta = -sibling's seq_delta
//!   ack_floor = own snd_nxt     (acks at/below it still feed smoltcp)
//!
//! and `splice_commit` drops the parked frames (the peer retransmits
//! them through the splice — they were never ACKed) and arms drain
//! mode: the session retires quietly once its emitted tail confirms,
//! or on the drain deadline. Any failure before install aborts to
//! `commit=false`: parked frames replay and the flow stays on the
//! smoltcp path.
//!
//! Window-scale rules: each leg's packets are decoded by the sibling
//! leg's negotiated shift, so promotion requires `peer_shift(legA) ==
//! own_shift(legB)` in both directions. Legs that negotiated TCP
//! timestamps get the option NOP-stripped on spliced frames; SACK block
//! edges are translated by the same ack delta as the ack field.

use super::*;

/// Attempt cadence: the first check runs once the relay has had time to
/// settle past connection ramp-up, then retries ride short intervals —
/// quiescence between data bursts can be fleeting on a busy flow.
const SPLICE_PROMOTE_DELAY: Duration = Duration::from_millis(50);
const SPLICE_PROMOTE_INTERVAL: Duration = Duration::from_millis(50);
const SPLICE_PROMOTE_ATTEMPTS: usize = 240;
const SPLICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// Flush convergence: legs poll until both pipelines drain to their
/// splice boundary — normally a few pump rounds; a stuck relay (a peer
/// that keeps its window shut on sub-boundary content) aborts the
/// attempt and the retry loop starts a fresh flush later.
const SPLICE_FLUSH_POLL: Duration = Duration::from_millis(2);
const SPLICE_FLUSH_BUDGET: Duration = Duration::from_secs(2);

/// Runtime switch — the splice fast path is the default on Linux but a
/// config or deployment can pin it off without a rebuild.
/// TEMP diag: env-gated stderr trace of promotion decisions.
pub(crate) fn splice_promotion_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("CLOUDSIDRA_XDP_TCP_SPLICE")
            .map(|value| !matches!(value.as_str(), "0" | "false" | "off"))
            .unwrap_or(true)
    })
}

/// Relay-side entry point: called once the upstream leg resolved to an
/// AF_XDP dial. `client_flow` is the accepted leg's key, `backend_flow`
/// the dialed leg's. Spawned detached — a refused or lost promotion
/// never disturbs the userspace relay.
#[cfg(target_os = "linux")]
pub(crate) fn maybe_promote_splice(
    client_flow: AfXdpTcpFlowKey,
    backend_flow: AfXdpTcpFlowKey,
    registry: Arc<AfXdpDialRegistry>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(SPLICE_PROMOTE_DELAY).await;
        for _ in 0..SPLICE_PROMOTE_ATTEMPTS {
            let attempt = try_promote_once(client_flow, backend_flow, &registry).await;
            match attempt {
                SpliceAttempt::Promoted => {
                    tracing::debug!(
                        "AF_XDP splice promoted client={}→{} backend={}→{}",
                        client_flow.peer_addr,
                        client_flow.local_addr,
                        backend_flow.local_addr,
                        backend_flow.peer_addr,
                    );
                    return;
                }
                SpliceAttempt::Ineligible => return,
                SpliceAttempt::NotQuiescent => {
                    tokio::time::sleep(SPLICE_PROMOTE_INTERVAL).await;
                }
            }
        }
    });
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum SpliceAttempt {
    Promoted,
    /// Structural refusal — retrying cannot help (wrong family, window
    /// scale mismatch, VLAN-tagged route, a leg already gone).
    Ineligible,
    /// A leg still has bytes in motion — safe to retry later.
    NotQuiescent,
}

#[cfg(target_os = "linux")]
async fn begin_leg(
    registry: &AfXdpDialRegistry,
    flow: AfXdpTcpFlowKey,
) -> Option<AfXdpSpliceLeg> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if !registry.session_request(&flow, AfXdpReactorRequest::SpliceBegin { flow, reply }) {
        return None;
    }
    tokio::time::timeout(SPLICE_REQUEST_TIMEOUT, rx)
        .await
        .ok()
        .and_then(|answer| answer.ok())
        .flatten()
}

/// One flush poll: `Some(snd_nxt)` once the leg drained its pre-splice
/// pipeline to the boundary.
#[cfg(target_os = "linux")]
async fn poll_leg(registry: &AfXdpDialRegistry, flow: AfXdpTcpFlowKey) -> Option<u32> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if !registry.session_request(&flow, AfXdpReactorRequest::SplicePoll { flow, reply }) {
        return None;
    }
    tokio::time::timeout(SPLICE_REQUEST_TIMEOUT, rx)
        .await
        .ok()
        .and_then(|answer| answer.ok())
        .flatten()
}

#[cfg(target_os = "linux")]
async fn end_leg(registry: &AfXdpDialRegistry, flow: AfXdpTcpFlowKey, commit: bool) -> bool {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if !registry.session_request(
        &flow,
        AfXdpReactorRequest::SpliceEnd {
            flow,
            commit,
            reply,
        },
    ) {
        return false;
    }
    tokio::time::timeout(SPLICE_REQUEST_TIMEOUT, rx)
        .await
        .ok()
        .and_then(|answer| answer.ok())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
async fn try_promote_once(
    client_flow: AfXdpTcpFlowKey,
    backend_flow: AfXdpTcpFlowKey,
    registry: &AfXdpDialRegistry,
) -> SpliceAttempt {
    // The datapath only rewrites IPv4; a v6 leg stays userspace.
    if !client_flow.local_addr.is_ipv4()
        || !client_flow.peer_addr.is_ipv4()
        || !backend_flow.local_addr.is_ipv4()
        || !backend_flow.peer_addr.is_ipv4()
    {
        return SpliceAttempt::Ineligible;
    }
    // Phase 1: arm flush on both legs. The boundary pins each leg's
    // receive edge — anything beyond it parks unacked while the
    // pre-splice pipeline drains below it.
    let Some(client_leg) = begin_leg(registry, client_flow).await else {
        return SpliceAttempt::NotQuiescent;
    };
    let Some(backend_leg) = begin_leg(registry, backend_flow).await else {
        let _ = end_leg(registry, client_flow, false).await;
        return SpliceAttempt::NotQuiescent;
    };
    // A packet's window field encodes the *sender's* receive window
    // shifted by the scale the sender announced in its SYN — so the
    // receiver decodes with the shift the packet's source announced.
    // A spliced client packet keeps the client's scale; the backend
    // decodes it with the scale the edge announced on the backend leg
    // (backend_leg's own shift). The datapath therefore rescales by
    // (sender_scale - edge_own_shift_on_sibling) per direction.
    let client_wnd_shift =
        client_leg.win_shift.0 as i32 - backend_leg.win_shift.1 as i32;
    let backend_wnd_shift =
        backend_leg.win_shift.0 as i32 - client_leg.win_shift.1 as i32;
    if client_wnd_shift > i8::MAX as i32
        || client_wnd_shift < i8::MIN as i32
        || backend_wnd_shift > i8::MAX as i32
        || backend_wnd_shift < i8::MIN as i32
    {
        let _ = end_leg(registry, client_flow, false).await;
        let _ = end_leg(registry, backend_flow, false).await;
        return SpliceAttempt::Ineligible;
    }
    if client_leg.vlan_tags != 0 || backend_leg.vlan_tags != 0 {
        let _ = end_leg(registry, client_flow, false).await;
        let _ = end_leg(registry, backend_flow, false).await;
        return SpliceAttempt::Ineligible;
    }
    // Phase 2: wait for both pipelines to flush to their boundaries.
    // `snd_nxt` at that instant is the first backend-wire position the
    // splice stream continues at; the boundary is the first client-wire
    // position it splices from.
    let flushed = async {
        let deadline = tokio::time::Instant::now() + SPLICE_FLUSH_BUDGET;
        loop {
            if tokio::time::Instant::now() > deadline {
                return None;
            }
            let (client_snd, backend_snd) = tokio::join!(
                poll_leg(registry, client_flow),
                poll_leg(registry, backend_flow),
            );
            if let (Some(c), Some(b)) = (client_snd, backend_snd) {
                return Some((c, b));
            }
            tokio::time::sleep(SPLICE_FLUSH_POLL).await;
        }
    };
    let Some((client_snd, backend_snd)) = flushed.await else {
        let _ = end_leg(registry, client_flow, false).await;
        let _ = end_leg(registry, backend_flow, false).await;
        return SpliceAttempt::NotQuiescent;
    };
    let manager = registry.manager();
    let client_key = linux::out_ct_key(
        client_flow.local_addr,
        client_flow.peer_addr,
        cloudsidra_edge_xdp_common::XDP_PROTO_TCP,
    );
    let backend_key = linux::out_ct_key(
        backend_flow.local_addr,
        backend_flow.peer_addr,
        cloudsidra_edge_xdp_common::XDP_PROTO_TCP,
    );
    let (Some(client_key), Some(backend_key)) = (client_key, backend_key) else {
        let _ = end_leg(registry, client_flow, false).await;
        let _ = end_leg(registry, backend_flow, false).await;
        return SpliceAttempt::Ineligible;
    };
    // Per-leg wire spaces: C.boundary is the first client-wire position
    // the splice owns; its backend-wire twin is B.snd (the tail owns
    // everything below it). seq_delta slides the ≥boundary range onto
    // [B.snd, …); the ack field maps the other direction's spliced
    // space back, so its delta is the sibling seq_delta negated. The
    // ack_floor routes acks still touching smoltcp's emitted tail to
    // userspace; once the tail confirms, every live ack exceeds it.
    let client_seq_delta = backend_snd.wrapping_sub(client_leg.rcv_nxt) as i32;
    let backend_seq_delta = client_snd.wrapping_sub(backend_leg.rcv_nxt) as i32;
    let client_value = cloudsidra_edge_xdp_common::XdpTcpSpliceValue {
        last_seen_ns: linux::monotonic_now_ns(),
        server_id: 0,
        new_dst_addr: octets(backend_flow.peer_addr),
        new_src_addr: octets(backend_flow.local_addr),
        next_hop_mac: backend_leg.next_hop_mac,
        new_dst_port_be: backend_flow.peer_addr.port().to_be(),
        new_src_port_be: backend_flow.local_addr.port().to_be(),
        ack_floor: client_snd,
        seq_delta: client_seq_delta,
        ack_delta: backend_seq_delta.wrapping_neg(),
        state: cloudsidra_edge_xdp_common::XDP_SPLICE_STATE_OPEN,
        ts_nego: client_leg.ts_nego as u8,
        wnd_shift: client_wnd_shift as i8,
        _pad: [0; 5],
    };
    let backend_value = cloudsidra_edge_xdp_common::XdpTcpSpliceValue {
        last_seen_ns: linux::monotonic_now_ns(),
        server_id: 0,
        new_dst_addr: octets(client_flow.peer_addr),
        new_src_addr: octets(client_flow.local_addr),
        next_hop_mac: client_leg.next_hop_mac,
        new_dst_port_be: client_flow.peer_addr.port().to_be(),
        new_src_port_be: client_flow.local_addr.port().to_be(),
        ack_floor: backend_snd,
        seq_delta: backend_seq_delta,
        ack_delta: client_seq_delta.wrapping_neg(),
        state: cloudsidra_edge_xdp_common::XDP_SPLICE_STATE_OPEN,
        ts_nego: backend_leg.ts_nego as u8,
        wnd_shift: backend_wnd_shift as i8,
        _pad: [0; 5],
    };
    if manager
        .upsert_splice(client_key, client_value)
        .and_then(|_| manager.upsert_splice(backend_key, backend_value))
        .is_err()
    {
        // Entries only half-installed — roll back whatever landed.
        let _ = manager.remove_splice(&client_key);
        let _ = manager.remove_splice(&backend_key);
        let _ = end_leg(registry, client_flow, false).await;
        let _ = end_leg(registry, backend_flow, false).await;
        return SpliceAttempt::NotQuiescent;
    }
    let client_ok = end_leg(registry, client_flow, true).await;
    let backend_ok = end_leg(registry, backend_flow, true).await;
    if client_ok && backend_ok {
        registry.register_splice(AfXdpSpliceLease {
            client_key,
            backend_key,
            backend_flow,
        });
        return SpliceAttempt::Promoted;
    }
    // A leg vanished mid-promotion: strip both entries. The surviving
    // leg unfreezes back to smoltcp; the dialed leg's port claim stays
    // registered (its owner row is untouched by splice rollback), so a
    // plain `release` is NOT correct here.
    let _ = manager.remove_splice(&client_key);
    let _ = manager.remove_splice(&backend_key);
    SpliceAttempt::NotQuiescent
}

/// v4 wire octets as stored in a splice value — same `::ffff:` embed
/// convention as XdpOutCtKey.
#[cfg(target_os = "linux")]
fn octets(addr: SocketAddr) -> [u8; 16] {
    match addr.ip() {
        IpAddr::V4(v4) => linux::v4_embed(v4),
        IpAddr::V6(v6) => v6.octets(),
    }
}
