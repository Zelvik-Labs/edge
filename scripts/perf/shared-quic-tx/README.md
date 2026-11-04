# Shared QUIC kernel TX component candidate

Production currently inherits `max_transmit_segments = 1`. Its segmented
fallback sends individual datagrams and must not simply advertise larger
batches: a later WouldBlock could follow an already-delivered prefix.

This Linux candidate uses the pinned quinn-udp native `try_send` via Tokio's
write-readiness registration. Linux uses one sendmsg for the complete transmit.
`try_send` preserves hard errors; the library's `send` converts some errors to
success and is deliberately not used here.

quinn-udp initialization also enables GRO. The candidate therefore requires
an unbound socket, initializes native state, disables and reads back UDP_GRO,
then permits bind. An already-bound socket is rejected before any option
changes. This prevents a receive-side GRO interval on a live plain demux.
No vendor/upstream code is modified.

Actions builds the standalone candidate with exactly the production direct
dependency versions and verifies IPv4/IPv6 GSO delivery, source addresses,
readiness clearing/recovery, GRO-off readback and visible EMSGSIZE without
delivery. The controls use plain datagram reception; they do not establish
production scheduling, throughput, hardware GSO/zero-copy or mixed-port safety.

Production integration is pending. It must retain the legacy/channel path,
preserve RX/demux ownership, handle capabilities and backpressure explicitly,
and pass independent same-binary node comparisons before default changes.
