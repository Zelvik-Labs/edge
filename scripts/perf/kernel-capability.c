// Minimal Linux capability probe: load/close XDP_PASS and open/close AF_XDP.
// No program attachment, interface modification, or traffic generation.
#include <errno.h>
#include <linux/bpf.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    const struct bpf_insn program[] = {
        {.code = BPF_ALU64 | BPF_MOV | BPF_K, .dst_reg = BPF_REG_0, .imm = XDP_PASS},
        {.code = BPF_JMP | BPF_EXIT},
    };
    const char license[] = "GPL";
    char log[4096] = {0};
    union bpf_attr attr = {0};
    attr.prog_type = BPF_PROG_TYPE_XDP;
    attr.insn_cnt = sizeof(program) / sizeof(program[0]);
    attr.insns = (uint64_t)(uintptr_t)program;
    attr.license = (uint64_t)(uintptr_t)license;
    attr.log_level = 1;
    attr.log_size = sizeof(log);
    attr.log_buf = (uint64_t)(uintptr_t)log;
    int program_fd = (int)syscall(__NR_bpf, BPF_PROG_LOAD, &attr, sizeof(attr));
    int program_errno = program_fd < 0 ? errno : 0;
    if (program_fd < 0 && log[0]) fprintf(stderr, "BPF verifier: %s\n", log);
    int socket_fd = socket(AF_XDP, SOCK_RAW | SOCK_CLOEXEC, 0);
    int socket_errno = socket_fd < 0 ? errno : 0;
    int cleanup_errno = 0;
    if (program_fd >= 0 && close(program_fd) < 0) cleanup_errno = errno;
    if (socket_fd >= 0 && close(socket_fd) < 0) cleanup_errno = errno;
    printf("{\"uid\":%u,\"euid\":%u,\"xdp_program_load\":%s,\"bpf_errno\":%d,\"af_xdp_socket\":%s,\"socket_errno\":%d,\"cleanup_errno\":%d}\n",
           getuid(), geteuid(), program_fd >= 0 ? "true" : "false", program_errno,
           socket_fd >= 0 ? "true" : "false", socket_errno, cleanup_errno);
    return program_fd >= 0 && socket_fd >= 0 && cleanup_errno == 0 ? 0 : 1;
}
