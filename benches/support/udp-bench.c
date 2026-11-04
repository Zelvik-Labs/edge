// Closed-loop UDP echo benchmark. Every completion validates the entire body.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <inttypes.h>
#include <poll.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static const unsigned char prefix[] = "xdp-udp-smoke:";
static volatile sig_atomic_t stopping;
static char failure[256];
static uint64_t received, sent;
static uint64_t measured_start, measured_end;

static uint64_t now_ns(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) { perror("clock_gettime"); exit(2); }
    return (uint64_t)t.tv_sec * 1000000000 + (uint64_t)t.tv_nsec;
}
static void stop(int signal_number) { (void)signal_number; stopping = 1; }
static int bad(const char *message) {
    snprintf(failure, sizeof(failure), "%s: %s", message, strerror(errno));
    return -1;
}
static void sequence(unsigned char *p, uint64_t value) {
    for (unsigned i = 0; i < 8; i++) p[8 + i] = (unsigned char)(value >> (56 - 8 * i));
}
static int address(struct sockaddr_in *a, const char *ip, unsigned port) {
    memset(a, 0, sizeof(*a)); a->sin_family = AF_INET; a->sin_port = htons((uint16_t)port);
    if (inet_pton(AF_INET, ip, &a->sin_addr) != 1) {
        snprintf(failure, sizeof(failure), "invalid IPv4 address"); return -1;
    }
    return 0;
}
static void cpu(FILE *f) {
    struct rusage usage;
    if (getrusage(RUSAGE_SELF, &usage)) { perror("getrusage"); exit(2); }
    fprintf(f, "\"cpu_user_seconds\":%.9f,\"cpu_system_seconds\":%.9f",
        usage.ru_utime.tv_sec + usage.ru_utime.tv_usec / 1e6,
        usage.ru_stime.tv_sec + usage.ru_stime.tv_usec / 1e6);
}
static int finish_file(FILE *f) {
    if (ferror(f)) { fclose(f); return bad("write result"); }
    return fclose(f) ? bad("close result") : 0;
}
static int marker(const char *path) {
    FILE *f = fopen(path, "w");
    if (!f) return bad("phase marker");
    fprintf(f, "{\"monotonic_ns\":%" PRIu64 "}\n", now_ns());
    return finish_file(f);
}
static int wait_go(const char *path) {
    uint64_t start = now_ns();
    while (access(path, F_OK)) {
        if (errno != ENOENT) return bad("phase release");
        if (now_ns() - start > UINT64_C(30000000000)) {
            snprintf(failure, sizeof(failure), "phase release timeout"); return -1;
        }
        struct timespec pause = {.tv_nsec = 1000000};
        if (nanosleep(&pause, NULL) && errno != EINTR) return bad("phase release wait");
    }
    return 0;
}
static int origin(const char *ip, unsigned port, const char *ready, const char *output,
                  bool corrupt) {
    struct sockaddr_in local;
    if (address(&local, ip, port)) return -1;
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return bad("origin socket");
    int result = 0;
    struct timeval timeout = {.tv_usec = 100000};
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout))) {
        result = bad("origin receive timeout"); goto close_fd;
    }
    if (bind(fd, (void *)&local, sizeof(local))) { result = bad("origin bind"); goto close_fd; }
    socklen_t length = sizeof(local);
    if (getsockname(fd, (void *)&local, &length)) { result = bad("origin address"); goto close_fd; }
    struct sigaction action = {.sa_handler = stop};
    if (sigemptyset(&action.sa_mask) || sigaction(SIGTERM, &action, NULL)
        || sigaction(SIGINT, &action, NULL)) { result = bad("signal handler"); goto close_fd; }
    FILE *r = fopen(ready, "w");
    if (!r) { result = bad("ready file"); goto close_fd; }
    fprintf(r, "{\"port\":%u}\n", ntohs(local.sin_port));
    if (finish_file(r)) { result = -1; goto close_fd; }
    unsigned char packet[4096];
    memcpy(packet, prefix, sizeof(prefix) - 1);
    // The port set proves actual upstream reuse; one origin IP is used.
    bool peers[65536] = {false}; unsigned peer_count = 0;
    while (!stopping) {
        struct sockaddr_in peer; socklen_t peer_len = sizeof(peer);
        ssize_t n = recvfrom(fd, packet + sizeof(prefix) - 1,
            sizeof(packet) - sizeof(prefix) + 1, MSG_TRUNC, (void *)&peer, &peer_len);
        if (n < 0) {
            if (errno == EINTR || errno == EAGAIN || errno == EWOULDBLOCK) continue;
            result = bad("origin receive"); break;
        }
        if ((size_t)n > sizeof(packet) - sizeof(prefix) + 1) {
            snprintf(failure, sizeof(failure), "origin datagram truncated"); result = -1; break;
        }
        received++;
        unsigned peer_port = ntohs(peer.sin_port);
        if (!peers[peer_port]) { peers[peer_port] = true; peer_count++; }
        if (corrupt && received == 1 && n) packet[sizeof(prefix) - 2 + (size_t)n] ^= 1;
        ssize_t wrote;
        do { wrote = sendto(fd, packet, (size_t)n + sizeof(prefix) - 1, 0,
                            (void *)&peer, peer_len); } while (wrote < 0 && errno == EINTR);
        if (wrote != n + (ssize_t)sizeof(prefix) - 1) { result = bad("origin send"); break; }
        sent++;
    }
    r = fopen(output, "w");
    if (!r) { result = bad("origin result"); goto close_fd; }
    fprintf(r, "{\"passed\":%s,\"received\":%" PRIu64 ",\"sent\":%" PRIu64
        ",\"distinct_peer_ports\":%u,", result ? "false" : "true", received, sent, peer_count);
    cpu(r); fprintf(r, "}\n");
    if (finish_file(r)) result = -1;
close_fd:
    if (close(fd)) result = bad("origin close");
    return result;
}

struct flow { int fd; uint64_t seq, start, completed; unsigned char request[2048]; };
static int issue(struct flow *flow, size_t payload) {
    sequence(flow->request, flow->seq++); flow->start = now_ns();
    ssize_t n;
    do { n = send(flow->fd, flow->request, payload, MSG_DONTWAIT); } while (n < 0 && errno == EINTR);
    if (n != (ssize_t)payload) return bad("client send");
    sent++; return 0;
}
static int phase(struct flow *flows, struct pollfd *fds, unsigned concurrency,
                 uint64_t iterations, size_t payload, uint64_t *samples, uint64_t *elapsed) {
    uint64_t count = 0, total = iterations * concurrency, index = 0;
    for (unsigned i = 0; i < concurrency; i++) flows[i].completed = 0;
    uint64_t start = now_ns();
    for (unsigned i = 0; i < concurrency && iterations; i++) if (issue(&flows[i], payload)) return -1;
    while (count < total) {
        int n = poll(fds, concurrency, 100);
        if (n < 0) { if (errno == EINTR) continue; return bad("client poll"); }
        uint64_t now = now_ns();
        for (unsigned i = 0; i < concurrency; i++) {
            if (flows[i].completed == iterations) continue;
            if (now - flows[i].start > UINT64_C(5000000000)) {
                snprintf(failure, sizeof(failure), "response timeout flow=%u", i); return -1;
            }
            if (fds[i].revents & (POLLERR | POLLHUP | POLLNVAL)) {
                snprintf(failure, sizeof(failure), "client descriptor error flow=%u events=%x", i,
                    (unsigned)fds[i].revents); return -1;
            }
            if (!(fds[i].revents & POLLIN)) continue;
            unsigned char reply[4096];
            ssize_t got = recv(flows[i].fd, reply, sizeof(reply), MSG_DONTWAIT | MSG_TRUNC);
            if (got < 0 && (errno == EAGAIN || errno == EINTR)) continue;
            if (got < 0) return bad("client receive");
            uint64_t end = now_ns();
            if (got != (ssize_t)(payload + sizeof(prefix) - 1)
                || memcmp(reply, prefix, sizeof(prefix) - 1)
                || memcmp(reply + sizeof(prefix) - 1, flows[i].request, payload)) {
                snprintf(failure, sizeof(failure), "echo length/body/sequence mismatch flow=%u seq=%" PRIu64,
                    i, flows[i].seq - 1); return -1;
            }
            received++; count++; flows[i].completed++;
            if (samples) samples[index++] = end - flows[i].start;
            if (flows[i].completed < iterations && issue(&flows[i], payload)) return -1;
        }
    }
    uint64_t end = now_ns();
    *elapsed = end - start;
    if (samples) { measured_start = start; measured_end = end; }
    return 0;
}
static int compare_u64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}
static uint64_t number(const char *text, uint64_t maximum) {
    char *end = NULL; errno = 0;
    uint64_t value = strtoull(text, &end, 10);
    if (!*text || text[0] == '-' || errno || *end || value > maximum) {
        fprintf(stderr, "invalid numeric argument\n"); exit(2);
    }
    return value;
}
static int client(const char *ip, unsigned port, unsigned concurrency, uint64_t iterations,
                  uint64_t warmup, size_t payload, const char *output, const char *sample_path,
                  const char *ready, const char *go, const char *done) {
    struct flow *flows = calloc(concurrency, sizeof(*flows));
    struct pollfd *fds = calloc(concurrency, sizeof(*fds));
    uint64_t count = iterations * concurrency;
    uint64_t *samples = calloc((size_t)count, sizeof(*samples));
    if (!flows || !fds || !samples) { free(flows); free(fds); free(samples); return bad("allocate client"); }
    for (unsigned i = 0; i < concurrency; i++) flows[i].fd = -1;
    struct sockaddr_in target; int result = address(&target, ip, port);
    for (unsigned i = 0; !result && i < concurrency; i++) {
        flows[i].fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
        if (flows[i].fd < 0) { result = bad("client socket"); break; }
        if (connect(flows[i].fd, (void *)&target, sizeof(target))) { result = bad("client connect"); break; }
        fds[i] = (struct pollfd){.fd = flows[i].fd, .events = POLLIN};
        memcpy(flows[i].request, "EDGEBENC", 8);
        for (size_t j = 16; j < payload; j++) flows[i].request[j] = (unsigned char)(j ^ i);
    }
    uint64_t elapsed = 0, warm_elapsed = 0;
    struct rusage before = {0}, after = {0};
    if (!result) result = phase(flows, fds, concurrency, warmup, payload, NULL, &warm_elapsed);
    if (!result && ready) result = marker(ready);
    if (!result && go) result = wait_go(go);
    if (!result && getrusage(RUSAGE_SELF, &before)) result = bad("timed CPU start");
    if (!result) result = phase(flows, fds, concurrency, iterations, payload, samples, &elapsed);
    if (!result && getrusage(RUSAGE_SELF, &after)) result = bad("timed CPU end");
    if (!result && done) result = marker(done);
    // Setup, sampling output, sorting and teardown do not enter client timing.
    for (unsigned i = 0; i < concurrency; i++) if (flows[i].fd >= 0 && close(flows[i].fd)) result = bad("client close");
    if (!result) {
        FILE *f = fopen(sample_path, "w");
        if (!f) result = bad("sample file");
        else {
            fprintf(f, "[");
            for (uint64_t i = 0; i < count; i++) fprintf(f, "%s%" PRIu64, i ? "," : "", samples[i]);
            fprintf(f, "]\n"); if (finish_file(f)) result = -1;
        }
    }
    long double sum = 0;
    if (!result) { for (uint64_t i = 0; i < count; i++) sum += samples[i]; qsort(samples, (size_t)count, sizeof(*samples), compare_u64); }
    FILE *f = fopen(output, "w");
    if (!f) result = bad("client result");
    else {
        fprintf(f, "{\"passed\":%s,\"error\":\"%s\",\"concurrency\":%u,\"payload\":%zu,"
            "\"iterations_per_flow\":%" PRIu64 ",\"warmup_per_flow\":%" PRIu64 ",\"sent\":%" PRIu64
            ",\"received\":%" PRIu64 ",", result ? "false" : "true", failure, concurrency,
            payload, iterations, warmup, sent, received);
        if (!result) fprintf(f, "\"elapsed_ns\":%" PRIu64 ",\"requests\":%" PRIu64
            ",\"requests_per_second\":%.6f,\"mean_rtt_ns\":%.6Lf,\"p50_rtt_ns\":%" PRIu64
            ",\"p99_rtt_ns\":%" PRIu64 ",", elapsed, count, (double)count * 1e9 / (double)elapsed,
            sum / count, samples[(count - 1) / 2], samples[(count * 99 + 99) / 100 - 1]);
        if (!result) fprintf(f, "\"timed_cpu_user_seconds\":%.9f,\"timed_cpu_system_seconds\":%.9f,",
            (after.ru_utime.tv_sec - before.ru_utime.tv_sec) + (after.ru_utime.tv_usec - before.ru_utime.tv_usec) / 1e6,
            (after.ru_stime.tv_sec - before.ru_stime.tv_sec) + (after.ru_stime.tv_usec - before.ru_stime.tv_usec) / 1e6);
        if (!result) fprintf(f, "\"timed_start_ns\":%" PRIu64 ",\"timed_end_ns\":%" PRIu64 ",",
            measured_start, measured_end);
        cpu(f); fprintf(f, "}\n"); if (finish_file(f)) result = -1;
    }
    free(flows); free(fds); free(samples); return result;
}
int main(int argc, char **argv) {
    // Positional arguments are deliberately strict; all invocations are retained.
    int result;
    if (argc == 7 && !strcmp(argv[1], "origin")
        && (!strcmp(argv[6], "valid") || !strcmp(argv[6], "corrupt"))) {
        result = origin(argv[2], (unsigned)number(argv[3], 65535), argv[4], argv[5], !strcmp(argv[6], "corrupt"));
    } else if ((argc == 10 && !strcmp(argv[1], "client"))
               || (argc == 13 && !strcmp(argv[1], "gated-client"))) {
        unsigned port = (unsigned)number(argv[3], 65535), c = (unsigned)number(argv[4], 128);
        uint64_t n = number(argv[5], 2000000), warm = number(argv[6], 2000000);
        size_t bytes = (size_t)number(argv[7], 1500);
        if (!port || port > 65535 || !c || c > 128 || !n || n > 2000000 / c
            || warm > 2000000 / c || bytes < 32 || bytes > 1500) {
            fprintf(stderr, "invalid benchmark bounds\n"); return 2;
        }
        result = client(argv[2], port, c, n, warm, bytes, argv[8], argv[9],
            argc == 13 ? argv[10] : NULL, argc == 13 ? argv[11] : NULL, argc == 13 ? argv[12] : NULL);
    } else {
        fprintf(stderr, "usage: udp-bench origin ip port ready.json output.json valid|corrupt\n"
            "       udp-bench client ip port concurrency iterations warmup payload output.json samples.json\n");
        return 2;
    }
    if (result) fprintf(stderr, "%s\n", failure);
    return result ? 1 : 0;
}
