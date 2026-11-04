// Reuse the verified fixture without modifying its standalone CLI. Its
// renamed, externally visible entry point also keeps all diagnostic helpers
// referenced under -Werror; the linker discards unused functions.
#define main edge_xsk_fixture_cli_main
#include "../afxdp-wake.c"
#undef main
#include <fcntl.h>

struct edge_context {
    struct fixture f;
    uint64_t completed;
};

struct edge_stats {
    uint64_t received, submitted, completed, fill_kicks;
};

static int edge_error(void) { return -(errno ? errno : EPROTO); }

const char *edge_xsk_failure_stage(void) { return failure_stage; }

void *edge_xsk_open(const char *rx, const char *peer, int *error) {
    struct edge_context *context=calloc(1,sizeof(*context));
    if(!context) { *error=-ENOMEM; failure_stage="context-allocation";return NULL; }
    struct fixture *f=&context->f;
    f->map_fd=f->program_fd=f->peer_fd=f->ep_fd=-1;
    if(setup(f,rx,peer)<0) {
        *error=edge_error();
        int release=cleanup(f);
        if(release) fprintf(stderr,"startup cleanup errno=%d\n",release);
        free(context);return NULL;
    }
    // The Rust wake variants own their reactor registration. Remove the
    // standalone C probe's epoll descriptor for every variant equally.
    if(close(f->ep_fd)<0) {
        *error=edge_error();failure_stage="close-probe-epoll";f->ep_fd=-1;
        int release=cleanup(f);
        if(release) fprintf(stderr,"startup cleanup errno=%d\n",release);
        free(context);return NULL;
    }
    f->ep_fd=-1;
    int sockets[]={xsk_socket__fd(f->socket),f->peer_fd};
    for(unsigned i=0;i<sizeof(sockets)/sizeof(sockets[0]);i++) {
        int flags=fcntl(sockets[i],F_GETFL,0);
        if(flags<0 || fcntl(sockets[i],F_SETFL,flags|O_NONBLOCK)<0) {
            *error=edge_error();failure_stage="nonblocking-fixture-socket";
            int release=cleanup(f);
            if(release) fprintf(stderr,"startup cleanup errno=%d\n",release);
            free(context);return NULL;
        }
    }
    *error=0;return context;
}

int edge_xsk_fd(void *context) { return xsk_socket__fd(((struct edge_context *)context)->f.socket); }
int edge_xsk_peer_fd(void *context) { return ((struct edge_context *)context)->f.peer_fd; }

int edge_xsk_peer_send(void *context,uint64_t sequence) {
    return send_peer(&((struct edge_context *)context)->f,sequence)<0?edge_error():1;
}

int edge_xsk_peer_receive(void *context,uint64_t sequence) {
    struct fixture *f=&((struct edge_context *)context)->f;
    unsigned char received[WIRE_SIZE+1];
    ssize_t n=recv(f->peer_fd,received,sizeof(received),MSG_DONTWAIT);
    if(n<0) {
        if(errno==EAGAIN || errno==EWOULDBLOCK) return 0;
        failure_stage="peer-receive";return edge_error();
    }
    if(validate(received,(uint32_t)n,sequence)<0) return edge_error();
    return 1;
}

// Functional controls only: observe a complete burst before deliberately
// clearing its edge, and consume RX without introducing TX write wakeups.
int edge_xsk_rx_pending(void *opaque) {
    struct fixture *f=&((struct edge_context *)opaque)->f;
    uint32_t index;
    uint32_t count=xsk_ring_cons__peek(&f->rx,FRAMES,&index);
    if(count) xsk_ring_cons__cancel(&f->rx,count);
    return (int)count;
}

int edge_xsk_receive_only(void *opaque,uint64_t expected) {
    struct fixture *f=&((struct edge_context *)opaque)->f;
    uint32_t index;
    if(!xsk_ring_cons__peek(&f->rx,1,&index)) return 0;
    const struct xdp_desc *rx=xsk_ring_cons__rx_desc(&f->rx,index);
    if(rx->addr>FRAMES*FRAME_SIZE-WIRE_SIZE || rx->options || f->held_count>=FRAMES) {
        failure_stage="rx-only-descriptor";return -EPROTO;
    }
    if(validate(xsk_umem__get_data(f->area,rx->addr),rx->len,expected)<0) return edge_error();
    f->held[f->held_count++]=rx->addr;
    xsk_ring_cons__release(&f->rx,1);f->received++;
    if(fill_held(f)<0) return edge_error();
    return 1;
}

int edge_xsk_reclaim(void *opaque) {
    struct edge_context *context=opaque;
    struct fixture *f=&context->f;
    uint32_t index;
    uint32_t count=xsk_ring_cons__peek(&f->comp,FRAMES,&index);
    for(uint32_t i=0;i<count;i++) {
        uint64_t address=*xsk_ring_cons__comp_addr(&f->comp,index+i);
        if(address>FRAMES*FRAME_SIZE-WIRE_SIZE || f->held_count>=FRAMES || context->completed>=f->transmitted) {
            failure_stage="completion-ownership";return -EPROTO;
        }
        f->held[f->held_count++]=address;context->completed++;
    }
    if(count) xsk_ring_cons__release(&f->comp,count);
    if(f->held_count && fill_held(f)<0) return edge_error();
    return (int)count;
}

// 0 means observed RX empty, 1 means a complete echo was submitted,
// 2 means TX backpressure. Only result 0 permits clearing RX readiness.
int edge_xsk_echo(void *opaque,uint64_t expected) {
    struct edge_context *context=opaque;
    struct fixture *f=&context->f;
    int reclaimed=edge_xsk_reclaim(context);
    if(reclaimed<0) return reclaimed;
    int ready=socket_poll(f,0);
    if(ready<0) return edge_error();
    if(!ready) return 0;
    uint32_t rx_index;
    if(!xsk_ring_cons__peek(&f->rx,1,&rx_index)) return 0;
    const struct xdp_desc *rx=xsk_ring_cons__rx_desc(&f->rx,rx_index);
    if(rx->addr>FRAMES*FRAME_SIZE-WIRE_SIZE || rx->options) {
        failure_stage="echo-rx-descriptor";return -EPROTO;
    }
    if(validate(xsk_umem__get_data(f->area,rx->addr),rx->len,expected)<0) return edge_error();
    uint32_t tx_index;
    if(xsk_ring_prod__reserve(&f->tx,1,&tx_index)!=1) {
        xsk_ring_cons__cancel(&f->rx,1);return 2;
    }
    *xsk_ring_prod__tx_desc(&f->tx,tx_index)=(struct xdp_desc){.addr=rx->addr,.len=rx->len};
    xsk_ring_cons__release(&f->rx,1);f->received++;
    xsk_ring_prod__submit(&f->tx,1);f->transmitted++;
    if(sendto(xsk_socket__fd(f->socket),NULL,0,MSG_DONTWAIT,NULL,0)<0) {
        failure_stage="echo-copy-tx-kick";return edge_error();
    }
    return 1;
}

void edge_xsk_stats(void *opaque,struct edge_stats *output) {
    struct edge_context *context=opaque;
    *output=(struct edge_stats){.received=context->f.received,.submitted=context->f.transmitted,.completed=context->completed,.fill_kicks=context->f.fill_kicks};
}

int edge_xsk_close(void *opaque) {
    struct edge_context *context=opaque;
    int error=cleanup(&context->f);free(context);
    if(error) failure_stage="native-cleanup";
    return -error;
}
