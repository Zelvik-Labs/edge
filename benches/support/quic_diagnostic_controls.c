/* Verify public frame decoding and the fixed trace-memory boundary. */
#include <assert.h>
#include <curl/curl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "quic_client_diagnostics.h"

int main(void) {
    QuicDiagnostic d = {0};
    d.bytes = malloc(DIAG_CAPACITY);
    assert(d.bytes);
    unsigned char hello[84] = {2, 0, 0, 80, 3, 3};
    hello[39] = 0x13; hello[40] = 1; /* cipher suite after empty session ID */
    hello[43] = 40; /* extension list length */
    hello[44] = 0; hello[45] = 51; hello[46] = 0; hello[47] = 36;
    hello[48] = 0; hello[49] = 29; hello[50] = 0; hello[51] = 32;
    assert(diagnostic_server_group(hello, sizeof(hello)) == 29);
    for (size_t n = 0; n < sizeof(hello); ++n) assert(!diagnostic_server_group(hello, n));
    diagnostic_message(0, 0, SSL3_RT_HANDSHAKE, hello, sizeof(hello), NULL, &d);
    assert(d.group_observations == 1 && d.current_group == 29);
    d.current_group = 0; hello[48] = 0x11; hello[49] = 0xec;
    diagnostic_message(0, 0, SSL3_RT_HANDSHAKE, hello, sizeof(hello), NULL, &d);
    assert(d.group_observations == 2 && d.last_group == 4588 && d.group_changes == 1);
    hello[51] = 31;
    assert(!diagnostic_server_group(hello, sizeof(hello)));
    const unsigned char packet_header[] = {0x45, 0x12, 0x34};
    diagnostic_message(0, 0, SSL3_RT_QUIC_PACKET, packet_header, sizeof(packet_header), NULL, &d);
    d.bytes[d.used] = 0;
    assert(strstr(d.bytes, "short=1 key_phase=1 pn_truncated=4660 pn_bytes=2 length=3 authenticated=false"));
    /* Type 0x1c, PROTOCOL_VIOLATION, triggering ACK, three-byte reason. */
    const unsigned char close[] = {28, 10, 2, 3, 'b', 'a', 'd'};
    diagnostic_message(1, 0, SSL3_RT_QUIC_FRAME_FULL, close, sizeof(close), NULL, &d);
    assert(d.current_closes == 1 && d.closes == 1);
    d.bytes[d.used] = 0;
    assert(strstr(d.bytes, "quic_close direction=write transport=1 code=10 frame=2 reason_bytes=3 reason_hex=626164"));
    const unsigned char app[] = {29, 0x41, 0, 0};
    diagnostic_message(0, 0, SSL3_RT_QUIC_FRAME_FULL, app, sizeof(app), NULL, &d);
    assert(d.current_closes == 2);
    d.bytes[d.used] = 0;
    assert(strstr(d.bytes, "transport=0 code=256 frame=0 reason_bytes=0"));
    for (size_t i = 0; i < sizeof(close); ++i)
        diagnostic_message(1, 0, SSL3_RT_QUIC_FRAME_FULL, close, i, NULL, &d);
    assert(d.current_closes == 2);
    size_t offset = 0;
    uint64_t value = 0;
    const unsigned char max[] = {255,255,255,255,255,255,255,255};
    assert(diagnostic_varint(max, sizeof(max), &offset, &value) && value == UINT64_C(4611686018427387903));
    for (size_t i = 0; i < sizeof(max); ++i) {
        offset = 0;
        assert(!diagnostic_varint(max, i, &offset, &value));
    }
    char *large = malloc(DIAG_CAPACITY + 100);
    assert(large);
    memset(large, 'a', DIAG_CAPACITY + 100);
    diagnostic_append(&d, large, DIAG_CAPACITY + 100);
    assert(d.used == DIAG_CAPACITY && d.dropped >= 100);
    diagnostic_append(&d, "tail", 4);
    assert(d.used == DIAG_CAPACITY && !memcmp(d.bytes + d.used - 4, "tail", 4));
    diagnostic_debug(NULL, CURLINFO_HEADER_IN, large, 100, &d);
    assert(!memcmp(d.bytes + d.used - 4, "tail", 4));
    SSL_CTX *ctx = SSL_CTX_new(TLS_method());
    assert(ctx && diagnostic_context(NULL, ctx, &d) == CURLE_OK && d.contexts == 1);
    SSL_CTX_free(ctx);
    d.used = 0;
    diagnostic_dump(&d, 0);
    free(d.bytes);
    free(large);
    puts("PASS: close frames, truncation, 62-bit varints, ring bound and public context hook");
    return 0;
}
