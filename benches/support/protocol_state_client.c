/* Native libcurl multi client. Fixed in-flight concurrency, one request per
 * connection at a time, exact body validation, optional connection-per-request.
 * CLI curl lacks CURLOPT_FRESH_CONNECT; do not simulate this with TCP keepalive.
 */
#include <curl/curl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#ifdef EDGE_QUIC_DIAGNOSTICS
#include "quic_client_diagnostics.h"
#endif

typedef struct {
    CURL *easy;
    char url[4096];
    const unsigned char *payload;
    size_t payload_size, received;
    int invalid_body;
    char error[CURL_ERROR_SIZE];
#ifdef EDGE_QUIC_DIAGNOSTICS
    QuicDiagnostic diagnostic;
#endif
} Transfer;

static double monotonic(void) {
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) { perror("clock_gettime"); exit(2); }
    return (double)t.tv_sec + t.tv_nsec / 1e9;
}

static size_t receive_body(char *data, size_t size, size_t count, void *context) {
    Transfer *t = context;
    size_t n = size * count;
    if (t->received > t->payload_size || n > t->payload_size - t->received ||
        memcmp(data, t->payload + t->received, n)) {
        t->invalid_body = 1;
        return 0;
    }
    t->received += n;
    return n;
}

static void easy_option(CURLcode result) {
    if (result != CURLE_OK) { fprintf(stderr,"curl option: %s\n",curl_easy_strerror(result)); exit(2); }
}
#define SET(e, key, value) easy_option(curl_easy_setopt((e),(key),(value)))

static void multi_option(CURLMcode result) {
    if (result != CURLM_OK) { fprintf(stderr,"curl multi: %s\n",curl_multi_strerror(result)); exit(2); }
}

int main(int argc, char **argv) {
    const char *url=NULL, *ca=NULL, *payload_path=NULL, *protocol=NULL;
    long connections=16, request_limit=0;
    double seconds=8;
    int fresh=0, unique=0, multiplex=0;
    for (int i=1;i<argc;i++) {
        if (!strcmp(argv[i],"--version")) { puts(curl_version()); return 0; }
        if (!strcmp(argv[i],"--fresh")) { fresh=1; continue; }
        if (!strcmp(argv[i],"--unique")) { unique=1; continue; }
        if (!strcmp(argv[i],"--multiplex")) { multiplex=1; continue; }
        if (i+1>=argc) { fprintf(stderr,"missing value\n"); return 2; }
        const char *key=argv[i++], *value=argv[i];
        if (!strcmp(key,"--url")) url=value;
        else if (!strcmp(key,"--cacert")) ca=value;
        else if (!strcmp(key,"--payload")) payload_path=value;
        else if (!strcmp(key,"--protocol")) protocol=value;
        else if (!strcmp(key,"--connections")) connections=strtol(value,NULL,10);
        else if (!strcmp(key,"--requests")) request_limit=strtol(value,NULL,10);
        else if (!strcmp(key,"--seconds")) seconds=strtod(value,NULL);
        else { fprintf(stderr,"unknown argument: %s\n",key); return 2; }
    }
    if (!url || !ca || !payload_path || !protocol || connections<1 || connections>4096 ||
        seconds<=0 || request_limit<0 || (fresh && multiplex)) { fprintf(stderr,"invalid arguments\n"); return 2; }
    long version = !strcmp(protocol,"h1") || !strcmp(protocol,"h1-tls") ? CURL_HTTP_VERSION_1_1 :
                   !strcmp(protocol,"h2") ? CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE :
                   !strcmp(protocol,"h3") ? CURL_HTTP_VERSION_3ONLY : -1;
    if (version<0) { fprintf(stderr,"unknown protocol\n"); return 2; }
    FILE *file=fopen(payload_path,"rb");
    if (!file) { perror("payload"); return 2; }
    if (fseek(file,0,SEEK_END)) { perror("fseek"); return 2; }
    long length=ftell(file);
    if (length<=0 || fseek(file,0,SEEK_SET)) { fprintf(stderr,"empty/invalid payload\n"); return 2; }
    unsigned char *payload=malloc((size_t)length);
    if (!payload || fread(payload,1,(size_t)length,file)!=(size_t)length) { fprintf(stderr,"payload read failed\n"); return 2; }
    fclose(file);
    easy_option(curl_global_init(CURL_GLOBAL_DEFAULT));
    CURLM *multi=curl_multi_init();
    if (!multi) { fprintf(stderr,"multi init failed\n"); return 2; }
    long connection_limit=multiplex ? 1L : connections;
    multi_option(curl_multi_setopt(multi,CURLMOPT_MAX_HOST_CONNECTIONS,connection_limit));
    multi_option(curl_multi_setopt(multi,CURLMOPT_MAX_TOTAL_CONNECTIONS,connection_limit));
    multi_option(curl_multi_setopt(multi,CURLMOPT_MAXCONNECTS,connection_limit));
    multi_option(curl_multi_setopt(multi,CURLMOPT_PIPELINING,
                                 (long)(multiplex ? CURLPIPE_MULTIPLEX : CURLPIPE_NOTHING)));
    Transfer *slots=calloc((size_t)connections,sizeof(*slots));
    if (!slots) { perror("slots"); return 2; }
    double started=monotonic(), deadline=started+seconds;
    long issued=0, completed=0, good=0, new_connections=0, active=0;
    for (long i=0;i<connections;i++) {
        Transfer *t=&slots[i];
        t->payload=payload; t->payload_size=(size_t)length;
        t->easy=curl_easy_init();
        if (!t->easy) { fprintf(stderr,"easy init failed\n"); return 2; }
        SET(t->easy,CURLOPT_HTTP_VERSION,version);
        SET(t->easy,CURLOPT_NOPROXY,"*");
        SET(t->easy,CURLOPT_IPRESOLVE,(long)CURL_IPRESOLVE_V4);
        SET(t->easy,CURLOPT_USERAGENT,"edge-protocol-state-matrix/1");
        SET(t->easy,CURLOPT_CAINFO,ca);
        SET(t->easy,CURLOPT_SSL_VERIFYPEER,1L);
        SET(t->easy,CURLOPT_SSL_VERIFYHOST,2L);
        /* Every newly established TLS connection performs a full handshake. */
        SET(t->easy,CURLOPT_SSL_SESSIONID_CACHE,0L);
        SET(t->easy,CURLOPT_FRESH_CONNECT,(long)fresh);
        SET(t->easy,CURLOPT_FORBID_REUSE,(long)fresh);
        SET(t->easy,CURLOPT_PIPEWAIT,(long)multiplex);
        SET(t->easy,CURLOPT_NOSIGNAL,1L);
        SET(t->easy,CURLOPT_CONNECTTIMEOUT_MS,5000L);
        SET(t->easy,CURLOPT_TIMEOUT_MS,10000L);
        SET(t->easy,CURLOPT_WRITEFUNCTION,receive_body);
        SET(t->easy,CURLOPT_WRITEDATA,t);
        SET(t->easy,CURLOPT_PRIVATE,t);
        SET(t->easy,CURLOPT_ERRORBUFFER,t->error);
#ifdef EDGE_QUIC_DIAGNOSTICS
        t->diagnostic.bytes = malloc(DIAG_CAPACITY);
        if (!t->diagnostic.bytes) { perror("diagnostic buffer"); return 2; }
        SET(t->easy,CURLOPT_SSL_CTX_FUNCTION,diagnostic_context);
        SET(t->easy,CURLOPT_SSL_CTX_DATA,&t->diagnostic);
        SET(t->easy,CURLOPT_VERBOSE,1L);
        SET(t->easy,CURLOPT_DEBUGFUNCTION,diagnostic_debug);
        SET(t->easy,CURLOPT_DEBUGDATA,&t->diagnostic);
#endif
        if (!request_limit || issued<request_limit) {
            int n=unique ? snprintf(t->url,sizeof(t->url),"%s/%ld-%ld",url,(long)getpid(),issued) :
                           snprintf(t->url,sizeof(t->url),"%s",url);
            if (n<0 || (size_t)n>=sizeof(t->url)) { fprintf(stderr,"URL too long\n"); return 2; }
            SET(t->easy,CURLOPT_URL,t->url);
            multi_option(curl_multi_add_handle(multi,t->easy)); issued++; active++;
        }
    }
    while (active) {
        int running=0;
        multi_option(curl_multi_perform(multi,&running));
        int left=0; CURLMsg *message;
        while ((message=curl_multi_info_read(multi,&left))) {
            if (message->msg!=CURLMSG_DONE) continue;
            Transfer *t=NULL; long code=0, actual_version=0, connects=0;
            curl_off_t total=0, first=0, bytes=0, connection_id=-1;
            easy_option(curl_easy_getinfo(message->easy_handle,CURLINFO_PRIVATE,&t));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_RESPONSE_CODE,&code));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_HTTP_VERSION,&actual_version));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_NUM_CONNECTS,&connects));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_TOTAL_TIME_T,&total));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_STARTTRANSFER_TIME_T,&first));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_SIZE_DOWNLOAD_T,&bytes));
            easy_option(curl_easy_getinfo(t->easy,CURLINFO_CONN_ID,&connection_id));
            int result=(int)message->data.result;
            if (result==CURLE_OK && (t->invalid_body || t->received!=t->payload_size)) result=CURLE_PARTIAL_FILE;
            const char *wire_version=actual_version==CURL_HTTP_VERSION_1_1 ? "1.1" :
                                     actual_version==CURL_HTTP_VERSION_2_0 ? "2" :
                                     actual_version==CURL_HTTP_VERSION_3 ? "3" : "0";
            printf("%ld\t%s\t%ld\t%lld\t%.9f\t%.9f\t%d\t%lld\n",code,wire_version,connects,
                   (long long)bytes,total/1e6,first/1e6,result,(long long)connection_id);
            if (result && completed-good<64) {
                fprintf(stderr,"transfer_error curl=%d http=%ld connection=%lld detail=%s\n",
                        result,code,(long long)connection_id,t->error);
#ifdef EDGE_QUIC_DIAGNOSTICS
                diagnostic_dump(&t->diagnostic,connection_id);
#endif
            }
            completed++; new_connections+=connects;
            if (!result && code==200 && actual_version==version) good++;
            /* 3ONLY negotiates CURL_HTTP_VERSION_3, not the 3ONLY enum value. */
            else if (!result && code==200 && version==CURL_HTTP_VERSION_3ONLY && actual_version==CURL_HTTP_VERSION_3) good++;
            else if (!result && code==200 && version==CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE && actual_version==CURL_HTTP_VERSION_2_0) good++;
            multi_option(curl_multi_remove_handle(multi,t->easy)); active--;
            if ((request_limit && issued<request_limit) || (!request_limit && monotonic()<deadline)) {
                t->received=0; t->invalid_body=0; t->error[0]='\0';
#ifdef EDGE_QUIC_DIAGNOSTICS
                t->diagnostic.used=0; t->diagnostic.dropped=0; t->diagnostic.current_closes=0;
#endif
                int n=unique ? snprintf(t->url,sizeof(t->url),"%s/%ld-%ld",url,(long)getpid(),issued) :
                               snprintf(t->url,sizeof(t->url),"%s",url);
                if (n<0 || (size_t)n>=sizeof(t->url)) { fprintf(stderr,"URL too long\n"); return 2; }
                SET(t->easy,CURLOPT_URL,t->url);
                multi_option(curl_multi_add_handle(multi,t->easy)); issued++; active++;
            }
        }
        if (active) multi_option(curl_multi_poll(multi,NULL,0,100,NULL));
    }
    double elapsed=monotonic()-started;
    fprintf(stderr,"{\"completed\":%ld,\"http200_valid_body\":%ld,\"new_connections\":%ld,\"seconds\":%.9f}\n",
            completed,good,new_connections,elapsed);
#ifdef EDGE_QUIC_DIAGNOSTICS
    uint64_t contexts=0, messages=0, frames=0, closes=0;
    for (long i=0;i<connections;i++) {
        contexts+=slots[i].diagnostic.contexts;
        messages+=slots[i].diagnostic.messages;
        frames+=slots[i].diagnostic.frames;
        closes+=slots[i].diagnostic.closes;
    }
    fprintf(stderr,"quic_diagnostic_summary contexts=%" PRIu64 " messages=%" PRIu64 " frames=%" PRIu64 " close_frames=%" PRIu64 "\n",
            contexts,messages,frames,closes);
#endif
    for (long i=0;i<connections;i++) curl_easy_cleanup(slots[i].easy);
#ifdef EDGE_QUIC_DIAGNOSTICS
    for (long i=0;i<connections;i++) free(slots[i].diagnostic.bytes);
#endif
    free(slots); curl_multi_cleanup(multi); curl_global_cleanup(); free(payload);
    return 0;
}
