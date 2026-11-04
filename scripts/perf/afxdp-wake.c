// Real AF_XDP copy-mode semantics probe. Run only in an owned network
// namespace with a fresh veth pair. No production interfaces or pins.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <bpf/bpf.h>
#include <bpf/libbpf.h>
#include <errno.h>
#include <linux/if_ether.h>
#include <linux/if_link.h>
#include <linux/if_packet.h>
#include <net/if.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>
#include <xdp/xsk.h>

#define FRAMES 64U
#define FRAME_SIZE 4096U
#define WIRE_SIZE 128U
#define ETH_PROBE 0x88b5
#define INS(c, d, s, o, i) ((struct bpf_insn){.code=(c), .dst_reg=(d), .src_reg=(s), .off=(o), .imm=(i)})

struct fixture {
    struct xsk_umem *umem;
    struct xsk_socket *socket;
    struct xsk_ring_prod fill, tx;
    struct xsk_ring_cons comp, rx;
    void *area;
    int map_fd, program_fd, peer_fd, ep_fd, ifindex;
    bool attached;
    uint64_t held[FRAMES];
    uint32_t held_count;
    uint64_t received, transmitted, fill_kicks;
};

static const char *failure_stage = "unknown";
#define CHECK(test, stage) do { if (!(test)) { failure_stage=(stage); if (!errno) errno=EPROTO; return -1; } } while(0)

static double seconds(clockid_t clock) {
    struct timespec t;
    if (clock_gettime(clock, &t)) return -1;
    return t.tv_sec + t.tv_nsec / 1e9;
}

static int socket_poll(struct fixture *f, int timeout) {
    struct pollfd p = {.fd=xsk_socket__fd(f->socket), .events=POLLIN};
    int rc;
    do { rc=poll(&p,1,timeout); } while (rc<0 && errno==EINTR);
    CHECK(rc>=0,"socket-poll");
    CHECK(!(p.revents & (POLLERR|POLLHUP|POLLNVAL)),"socket-poll-events");
    return rc;
}

static int fill_held(struct fixture *f) {
    uint32_t index;
    CHECK(f->held_count>0,"fill-has-owned-frames");
    CHECK(xsk_ring_prod__reserve(&f->fill,f->held_count,&index)==f->held_count,"fill-reserve");
    for (uint32_t i=0;i<f->held_count;i++)
        *xsk_ring_prod__fill_addr(&f->fill,index+i)=f->held[i];
    xsk_ring_prod__submit(&f->fill,f->held_count);
    f->held_count=0;
    if (xsk_ring_prod__needs_wakeup(&f->fill)) {
        f->fill_kicks++;
        CHECK(socket_poll(f,0)>=0,"fill-wakeup");
    }
    return 0;
}

static void wire(unsigned char *frame, uint64_t seq) {
    memset(frame,0x5a,WIRE_SIZE);
    const unsigned char dst[6]={0x02,0,0,0,0,1},src[6]={0x02,0,0,0,0,2};
    memcpy(frame,dst,6); memcpy(frame+6,src,6);
    uint16_t proto=htons(ETH_PROBE);
    memcpy(frame+12,&proto,2); memcpy(frame+14,&seq,8);
}

static int validate(const unsigned char *frame, uint32_t len, uint64_t seq) {
    unsigned char expected[WIRE_SIZE]; wire(expected,seq);
    CHECK(len==WIRE_SIZE && memcmp(frame,expected,WIRE_SIZE)==0,"wire-content");
    return 0;
}

static int send_peer(struct fixture *f, uint64_t seq) {
    unsigned char frame[WIRE_SIZE]; wire(frame,seq);
    CHECK(send(f->peer_fd,frame,sizeof(frame),0)==(ssize_t)sizeof(frame),"peer-send");
    return 0;
}

static int drain(struct fixture *f, uint32_t count, uint64_t first) {
    double deadline=seconds(CLOCK_MONOTONIC)+5;
    uint32_t done=0;
    while (done<count) {
        uint32_t index;
        uint32_t n=xsk_ring_cons__peek(&f->rx,count-done,&index);
        if (!n) {
            CHECK(seconds(CLOCK_MONOTONIC)<deadline,"rx-deadline");
            CHECK(socket_poll(f,10)>=0,"rx-wait");
            continue;
        }
        for (uint32_t i=0;i<n;i++) {
            const struct xdp_desc *d=xsk_ring_cons__rx_desc(&f->rx,index+i);
            CHECK(d->addr<=FRAMES*FRAME_SIZE-WIRE_SIZE,"rx-umem-address");
            CHECK(!d->options,"rx-descriptor-options");
            CHECK(validate(xsk_umem__get_data(f->area,d->addr),d->len,first+done+i)==0,"rx-validate");
            CHECK(f->held_count<FRAMES,"rx-ownership-capacity");
            f->held[f->held_count++]=d->addr;
        }
        xsk_ring_cons__release(&f->rx,n);
        f->received+=n;done+=n;
    }
    return 0;
}

static int ep_wait(struct fixture *f, int timeout, uint32_t *events) {
    struct epoll_event event;
    int n;
    do {n=epoll_wait(f->ep_fd,&event,1,timeout);} while(n<0 && errno==EINTR);
    CHECK(n>=0,"epoll-wait");
    *events=n?event.events:0;
    CHECK(!(*events & (EPOLLERR|EPOLLHUP)),"epoll-error-events");
    return n;
}

static int setup(struct fixture *f, const char *rx_name, const char *peer_name) {
    struct rlimit lock;
    CHECK(getrlimit(RLIMIT_MEMLOCK,&lock)==0,"memlock-read");
    const rlim_t needed=FRAMES*FRAME_SIZE+1024*1024;
    if(lock.rlim_cur<needed) {
        CHECK(lock.rlim_max>=needed,"memlock-hard-limit");
        lock.rlim_cur=needed;
        CHECK(setrlimit(RLIMIT_MEMLOCK,&lock)==0,"memlock-raise-to-umem-size");
    }
    f->ifindex=if_nametoindex(rx_name);
    unsigned peer_index=if_nametoindex(peer_name);
    CHECK(f->ifindex>0 && peer_index>0,"interface-index");
    int rc=posix_memalign(&f->area,FRAME_SIZE,FRAMES*FRAME_SIZE);
    if (rc) errno=rc;
    CHECK(!rc,"umem-allocation");
    struct xsk_umem_config u={.fill_size=FRAMES,.comp_size=FRAMES,.frame_size=FRAME_SIZE};
    rc=xsk_umem__create(&f->umem,f->area,FRAMES*FRAME_SIZE,&f->fill,&f->comp,&u);
    if (rc) errno=-rc;
    CHECK(!rc,"umem-create");
    struct xsk_socket_config s={.rx_size=FRAMES,.tx_size=FRAMES,.libbpf_flags=XSK_LIBBPF_FLAGS__INHIBIT_PROG_LOAD,.xdp_flags=XDP_FLAGS_SKB_MODE,.bind_flags=XDP_COPY|XDP_USE_NEED_WAKEUP};
    rc=xsk_socket__create(&f->socket,rx_name,0,f->umem,&f->rx,&f->tx,&s);
    if (rc) errno=-rc;
    CHECK(!rc,"xsk-copy-bind");
    f->map_fd=bpf_map_create(BPF_MAP_TYPE_XSKMAP,"wake_probe",4,4,1,NULL);
    CHECK(f->map_fd>=0,"xsk-map-create");
    int key=0,fd=xsk_socket__fd(f->socket);
    CHECK(bpf_map_update_elem(f->map_fd,&key,&fd,BPF_ANY)==0,"xsk-map-update");
    const struct bpf_insn prog[]={
        INS(BPF_LDX|BPF_W|BPF_MEM,2,1,0,0),
        INS(BPF_LDX|BPF_W|BPF_MEM,3,1,4,0),
        INS(BPF_ALU64|BPF_MOV|BPF_X,4,2,0,0),
        INS(BPF_ALU64|BPF_ADD|BPF_K,4,0,0,14),
        INS(BPF_JMP|BPF_JGT|BPF_X,4,3,8,0),
        INS(BPF_LDX|BPF_H|BPF_MEM,4,2,12,0),
        INS(BPF_JMP|BPF_JNE|BPF_K,4,0,6,htons(ETH_PROBE)),
        INS(BPF_LD|BPF_DW|BPF_IMM,1,BPF_PSEUDO_MAP_FD,0,f->map_fd),
        INS(0,0,0,0,0),
        INS(BPF_ALU64|BPF_MOV|BPF_K,2,0,0,0),
        INS(BPF_ALU64|BPF_MOV|BPF_K,3,0,0,XDP_DROP),
        INS(BPF_JMP|BPF_CALL,0,0,0,BPF_FUNC_redirect_map),
        INS(BPF_JMP|BPF_EXIT,0,0,0,0),
        INS(BPF_ALU64|BPF_MOV|BPF_K,0,0,0,XDP_PASS),
        INS(BPF_JMP|BPF_EXIT,0,0,0,0),
    };
    char log[16384]={0};
    struct bpf_prog_load_opts load={.sz=sizeof(load),.log_level=1,.log_buf=log,.log_size=sizeof(log)};
    f->program_fd=bpf_prog_load(BPF_PROG_TYPE_XDP,"wake_probe","GPL",prog,sizeof(prog)/sizeof(prog[0]),&load);
    if(f->program_fd<0 && log[0]) fprintf(stderr,"verifier: %s\n",log);
    CHECK(f->program_fd>=0,"xdp-program-load");
    CHECK(bpf_xdp_attach(f->ifindex,f->program_fd,XDP_FLAGS_SKB_MODE|XDP_FLAGS_UPDATE_IF_NOEXIST,NULL)==0,"xdp-attach-owned-veth");
    f->attached=true;
    f->peer_fd=socket(AF_PACKET,SOCK_RAW|SOCK_CLOEXEC,htons(ETH_PROBE));
    CHECK(f->peer_fd>=0,"peer-socket");
    int one=1;
    CHECK(setsockopt(f->peer_fd,SOL_PACKET,PACKET_IGNORE_OUTGOING,&one,sizeof(one))==0,"ignore-peer-outgoing");
    struct sockaddr_ll peer={.sll_family=AF_PACKET,.sll_protocol=htons(ETH_PROBE),.sll_ifindex=(int)peer_index};
    CHECK(bind(f->peer_fd,(struct sockaddr *)&peer,sizeof(peer))==0,"peer-bind");
    struct timeval timeout={.tv_sec=5};
    CHECK(setsockopt(f->peer_fd,SOL_SOCKET,SO_RCVTIMEO,&timeout,sizeof(timeout))==0,"peer-recv-timeout");
    CHECK(setsockopt(f->peer_fd,SOL_SOCKET,SO_SNDTIMEO,&timeout,sizeof(timeout))==0,"peer-send-timeout");
    f->ep_fd=epoll_create1(EPOLL_CLOEXEC);
    CHECK(f->ep_fd>=0,"epoll-create");
    struct epoll_event event={.events=EPOLLIN|EPOLLET,.data.fd=fd};
    CHECK(epoll_ctl(f->ep_fd,EPOLL_CTL_ADD,fd,&event)==0,"epoll-add-readable-only");
    for(uint32_t i=0;i<FRAMES;i++) f->held[f->held_count++]=(uint64_t)i*FRAME_SIZE;
    return fill_held(f);
}

static int exercise(struct fixture *f) {
    uint32_t events=0;
    struct epoll_event interest={.events=EPOLLIN|EPOLLOUT|EPOLLET,.data.fd=xsk_socket__fd(f->socket)};
    CHECK(epoll_ctl(f->ep_fd,EPOLL_CTL_MOD,xsk_socket__fd(f->socket),&interest)==0,"epoll-read-write-control");
    CHECK(ep_wait(f,0,&events)>=0,"read-write-first-event");
    fprintf(stderr,"read_write_edge_initial_events=%u\n",events);
    CHECK(ep_wait(f,20,&events)>=0,"read-write-followup-event");
    fprintf(stderr,"read_write_edge_idle_followup_events=%u\n",events);
    interest.events=EPOLLIN|EPOLLET;
    CHECK(epoll_ctl(f->ep_fd,EPOLL_CTL_MOD,xsk_socket__fd(f->socket),&interest)==0,"epoll-readable-only-restore");
    double cpu_before=seconds(CLOCK_PROCESS_CPUTIME_ID);
    CHECK(ep_wait(f,200,&events)==0,"idle-readable-only");
    double idle_cpu=seconds(CLOCK_PROCESS_CPUTIME_ID)-cpu_before;
    fprintf(stderr,"idle_readable_only_cpu_seconds=%.9f\n",idle_cpu);
    for(uint64_t i=0;i<8;i++) CHECK(send_peer(f,i)==0,"burst-send");
    CHECK(ep_wait(f,5000,&events)==1 && (events&EPOLLIN),"rx-epoll-event");
    CHECK(drain(f,1,0)==0,"partial-drain");
    CHECK(ep_wait(f,0,&events)>=0,"partial-drain-edge-observation");
    fprintf(stderr,"partial_drain_second_edge_events=%u\n",events);
    // With edge-triggered registration, unread RX is not a new edge. Keep
    // readiness owned while draining; a second wait must not be required.
    CHECK(drain(f,7,1)==0,"remaining-burst-drain");
    CHECK(ep_wait(f,20,&events)==0,"drained-rx-idles");
    for(uint64_t i=8;i<16;i++) CHECK(send_peer(f,i)==0,"poll-control-send");
    CHECK(socket_poll(f,5000)==1,"poll-control-ready");
    CHECK(drain(f,8,8)==0,"poll-control-drain");
    for(uint64_t i=16;i<FRAMES;i++) CHECK(send_peer(f,i)==0,"exhaust-fill-send");
    CHECK(drain(f,FRAMES-16,16)==0,"exhaust-fill-drain");
    CHECK(f->held_count==FRAMES,"all-fill-frames-returned");
    struct xdp_statistics before={0},after={0};socklen_t len=sizeof(before);
    CHECK(getsockopt(xsk_socket__fd(f->socket),SOL_XDP,XDP_STATISTICS,&before,&len)==0,"stats-before-starvation");
    CHECK(send_peer(f,FRAMES)==0,"starved-send");
    // Consume a possible stale edge, then require no data. RX ring peek
    // below is the authoritative check; epoll may report the prior burst.
    CHECK(socket_poll(f,20)==0,"fill-starved-no-rx");
    uint32_t index;
    CHECK(xsk_ring_cons__peek(&f->rx,1,&index)==0,"starved-rx-empty");
    len=sizeof(after);
    CHECK(getsockopt(xsk_socket__fd(f->socket),SOL_XDP,XDP_STATISTICS,&after,&len)==0,"stats-after-starvation");
    CHECK(after.rx_dropped>before.rx_dropped,"starvation-observable-drop");
    CHECK(fill_held(f)==0,"refill-after-starvation");
    CHECK(send_peer(f,FRAMES+1)==0,"recovery-send");
    CHECK(ep_wait(f,5000,&events)==1 && (events&EPOLLIN),"recovery-epoll");
    CHECK(drain(f,1,FRAMES+1)==0,"recovery-content");
    CHECK(f->held_count==1,"tx-owned-frame");
    uint64_t tx_address=f->held[--f->held_count];
    wire(xsk_umem__get_data(f->area,tx_address),1000);
    CHECK(xsk_ring_prod__reserve(&f->tx,1,&index)==1,"tx-reserve");
    struct xdp_desc *tx=xsk_ring_prod__tx_desc(&f->tx,index);
    *tx=(struct xdp_desc){.addr=tx_address,.len=WIRE_SIZE};
    xsk_ring_prod__submit(&f->tx,1);
    CHECK(sendto(xsk_socket__fd(f->socket),NULL,0,MSG_DONTWAIT,NULL,0)==0,"copy-tx-kick");
    unsigned char received[WIRE_SIZE+1];
    ssize_t n=recv(f->peer_fd,received,sizeof(received),0);
    CHECK(n>=0 && validate(received,(uint32_t)n,1000)==0,"tx-peer-content");
    double deadline=seconds(CLOCK_MONOTONIC)+5;
    while(!xsk_ring_cons__peek(&f->comp,1,&index)) {
        CHECK(seconds(CLOCK_MONOTONIC)<deadline,"tx-completion-deadline");
        CHECK(socket_poll(f,1)>=0,"tx-completion-wait");
    }
    CHECK(*xsk_ring_cons__comp_addr(&f->comp,index)==tx_address,"tx-completion-address");
    xsk_ring_cons__release(&f->comp,1);f->held[f->held_count++]=tx_address;f->transmitted++;
    CHECK(ep_wait(f,20,&events)==0,"rx-idle-after-tx-completion");
    fprintf(stderr,"starvation_rx_drop_delta=%llu fill_kicks=%llu\n",(unsigned long long)(after.rx_dropped-before.rx_dropped),(unsigned long long)f->fill_kicks);
    return 0;
}

static int cleanup(struct fixture *f) {
    int error=0;
    if(f->attached) {
        struct bpf_xdp_attach_opts opts={.sz=sizeof(opts),.old_prog_fd=f->program_fd};
        int rc=bpf_xdp_detach(f->ifindex,XDP_FLAGS_SKB_MODE,&opts);
        if(rc) error=-rc;
    }
    if(f->socket) xsk_socket__delete(f->socket);
    if(f->umem) {int rc=xsk_umem__delete(f->umem);if(rc) error=-rc;}
    for(int *fd=(int[]){f->ep_fd,f->peer_fd,f->program_fd,f->map_fd},i=0;i<4;i++)
        if(fd[i]>=0 && close(fd[i])<0) error=errno;
    free(f->area);
    return error;
}

int main(int argc,char **argv) {
    if(argc!=3) {fprintf(stderr,"usage: wake-probe owned-rx-veth owned-peer-veth\n");return 2;}
    struct fixture f={.map_fd=-1,.program_fd=-1,.peer_fd=-1,.ep_fd=-1};
    int rc=setup(&f,argv[1],argv[2]);
    if(!rc) rc=exercise(&f);
    int saved=rc?errno:0;
    int cleanup_errno=cleanup(&f);
    printf("{\"passed\":%s,\"failure_stage\":\"%s\",\"errno\":%d,\"cleanup_errno\":%d,\"received_frames\":%llu,\"transmitted_frames\":%llu,\"mode\":\"veth-skb-copy\",\"fill_kicks\":%llu}\n",(!rc&&!cleanup_errno)?"true":"false",rc?failure_stage:"none",saved,cleanup_errno,(unsigned long long)f.received,(unsigned long long)f.transmitted,(unsigned long long)f.fill_kicks);
    return rc||cleanup_errno?1:0;
}
