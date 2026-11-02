#include <errno.h>
#include <linux/io_uring.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(void) {
    struct io_uring_params params;
    memset(&params, 0, sizeof(params));
    long fd = syscall(__NR_io_uring_setup, 8U, &params);
    if (fd < 0) {
        int error = errno;
        printf("{\"available\":false,\"errno\":%d,\"error\":\"%s\"}\n", error, strerror(error));
        return 1;
    }
    printf("{\"available\":true,\"features\":%u,\"sq_entries\":%u,\"cq_entries\":%u}\n",
           params.features, params.sq_entries, params.cq_entries);
    return close((int)fd) == 0 ? 0 : 2;
}
