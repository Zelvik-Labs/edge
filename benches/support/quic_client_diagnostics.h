/* Explicit diagnostic build only. Observe protocol frames without payloads or
 * TLS secrets. Keep the last 64KiB per active slot; emit it only on failure. */
#include <inttypes.h>
#include <openssl/ssl.h>

#ifndef SSL3_RT_QUIC_FRAME_FULL
#error "QUIC frame diagnostics require OpenSSL >= 3.2"
#endif

enum { DIAG_CAPACITY = 65536 };
typedef struct {
    char *bytes;
    size_t used, dropped;
    uint64_t contexts, messages, frames, closes, current_closes;
} QuicDiagnostic;

static void diagnostic_append(QuicDiagnostic *d, const char *bytes, size_t length) {
    if (length >= DIAG_CAPACITY) {
        d->dropped += d->used + length - DIAG_CAPACITY;
        memcpy(d->bytes, bytes + length - DIAG_CAPACITY, DIAG_CAPACITY);
        d->used = DIAG_CAPACITY;
        return;
    }
    if (d->used + length > DIAG_CAPACITY) {
        size_t discard = d->used + length - DIAG_CAPACITY;
        memmove(d->bytes, d->bytes + discard, d->used - discard);
        d->used -= discard;
        d->dropped += discard;
    }
    memcpy(d->bytes + d->used, bytes, length);
    d->used += length;
}

static int diagnostic_varint(const unsigned char *bytes, size_t length,
                             size_t *offset, uint64_t *value) {
    if (*offset >= length) return 0;
    size_t count = (size_t)1 << (bytes[*offset] >> 6);
    if (count > length - *offset) return 0;
    *value = bytes[(*offset)++] & 63;
    for (size_t i = 1; i < count; ++i) *value = (*value << 8) | bytes[(*offset)++];
    return 1;
}

static void diagnostic_message(int writing, int version, int type,
                               const void *buffer, size_t length, SSL *ssl, void *context) {
    QuicDiagnostic *d = context;
    const unsigned char *bytes = buffer;
    (void)version;
    (void)ssl;
    d->messages++;
    if (type == SSL3_RT_QUIC_PACKET && length) {
        /* OpenSSL 3.5.7 QRX supplies only the unprotected packet header, before
         * payload authentication. Record flags and truncated PN, never payload
         * or tokens. The event alone is not proof of an authenticated packet. */
        char line[256];
        unsigned pn_bytes = (bytes[0] & 3) + 1;
        uint64_t pn = 0;
        if (length < pn_bytes) return;
        for (size_t i = length - pn_bytes; i < length; ++i) pn = (pn << 8) | bytes[i];
        int written = snprintf(line, sizeof(line), "quic_packet_header direction=%s short=%d key_phase=%u pn_truncated=%" PRIu64 " pn_bytes=%u length=%zu authenticated=false\n",
            writing ? "write" : "read", !(bytes[0] & 128), !!(bytes[0] & 4), pn, pn_bytes, length);
        if (written < 0 || (size_t)written >= sizeof(line)) abort();
        diagnostic_append(d, line, (size_t)written);
        return;
    }
    if (type != SSL3_RT_QUIC_FRAME_FULL && type != SSL3_RT_QUIC_FRAME_HEADER) return;
    d->frames++;
    char line[1024];
    size_t offset = 0;
    uint64_t frame;
    if (!diagnostic_varint(bytes, length, &offset, &frame)) return;
    if (type == SSL3_RT_QUIC_FRAME_FULL && (frame == 48 || frame == 49)) return;
    int written = snprintf(line, sizeof(line), "quic_frame direction=%s kind=%s type=%" PRIu64 " length=%zu header_hex=",
                           writing ? "write" : "read", type == SSL3_RT_QUIC_FRAME_FULL ? "full" : "header", frame, length);
    if (written < 0 || (size_t)written >= sizeof(line)) abort();
    size_t used = (size_t)written;
    /* Frame headers and protocol control fields only. Stream/CRYPTO payloads
     * are absent from HEADER callbacks; do not record DATAGRAM/PACKET data. */
    size_t prefix = length < 64 ? length : 64;
    for (size_t i = 0; i < prefix; ++i) {
        snprintf(line + used, sizeof(line) - used, "%02x", bytes[i]);
        used += 2;
    }
    line[used++] = '\n';
    diagnostic_append(d, line, used);
    if (type == SSL3_RT_QUIC_FRAME_FULL && (frame == 28 || frame == 29)) {
        uint64_t code, causing_frame = 0, reason_length;
        if (!diagnostic_varint(bytes, length, &offset, &code) ||
            (frame == 28 && !diagnostic_varint(bytes, length, &offset, &causing_frame)) ||
            !diagnostic_varint(bytes, length, &offset, &reason_length) || reason_length > length - offset) {
            const char malformed[] = "quic_close_decode_failed\n";
            diagnostic_append(d, malformed, sizeof(malformed) - 1);
            return;
        }
        written = snprintf(line, sizeof(line), "quic_close direction=%s transport=%d code=%" PRIu64 " frame=%" PRIu64 " reason_bytes=%" PRIu64 " reason_hex=",
                           writing ? "write" : "read", frame == 28, code, causing_frame, reason_length);
        if (written < 0 || (size_t)written >= sizeof(line)) abort();
        used = (size_t)written;
        size_t reason_prefix = reason_length < 256 ? (size_t)reason_length : 256;
        for (size_t i = 0; i < reason_prefix; ++i) {
            snprintf(line + used, sizeof(line) - used, "%02x", bytes[offset + i]);
            used += 2;
        }
        line[used++] = '\n';
        diagnostic_append(d, line, used);
        d->closes++;
        d->current_closes++;
    }
}

static CURLcode diagnostic_context(CURL *easy, void *tls_context, void *context) {
    QuicDiagnostic *d = context;
    (void)easy;
    d->contexts++;
    SSL_CTX_set_msg_callback(tls_context, diagnostic_message);
    SSL_CTX_set_msg_callback_arg((SSL_CTX *)tls_context, d);
    return CURLE_OK;
}

static int diagnostic_debug(CURL *easy, curl_infotype type, char *data, size_t length, void *context) {
    (void)easy;
    if (type == CURLINFO_TEXT) diagnostic_append(context, data, length);
    return 0;
}

static void diagnostic_dump(QuicDiagnostic *d, curl_off_t connection) {
    fprintf(stderr, "quic_trace_begin connection=%lld dropped_bytes=%zu close_frames=%" PRIu64 "\n",
            (long long)connection, d->dropped, d->current_closes);
    if (fwrite(d->bytes, 1, d->used, stderr) != d->used) { perror("diagnostic write"); exit(2); }
    fprintf(stderr, "\nquic_trace_end connection=%lld\n", (long long)connection);
}
