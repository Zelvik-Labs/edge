# Pingora 0.9.0 controlled fork

## Upstream baseline

- Project: https://github.com/cloudflare/pingora
- Tag: `0.9.0`
- Commit: `702f69015e53f7244d6ad2e743de571d859a70a4`
- Previous upstream tag: `0.8.1`

The 0.9.0 snapshot replaces the previous baseline. Existing Edge extensions
are retained only where 0.9.0 does not provide equivalent functionality.
This is not an API compatibility layer: Edge uses the 0.9 cache-key, purge,
header mutation, runtime TLS configuration, and H2 acceptance APIs directly.

## Retained extensions

| Extension | Representative files | Reason |
| --- | --- | --- |
| File-backed cache bodies | `pingora-http/src/lib.rs`, `pingora-cache/src/storage.rs`, `pingora-core/src/protocols/{mod.rs,l4/stream.rs,http/server.rs,http/v1/server.rs}`, `pingora-proxy/src/{proxy_cache.rs,proxy_trait.rs}` | Preserve the existing Linux kernel TCP sendfile path and its byte accounting/rate-limiting callback. TLS, virtual transports, range-filtered responses, and attached downstream modules retain the byte-stream path. Transfer errors propagate instead of replaying possibly partially sent bytes. |
| Request parsing error policy | `pingora-proxy/src/{lib.rs,proxy_trait.rs}` | Let Edge account for malformed requests and choose the error response before a Session exists. Logs retain their actual severity; global warning demotion is not retained. |
| Custom connector construction | `pingora-proxy/src/lib.rs` | Edge's AF_XDP/custom transport needs a directly usable proxy, not only a listening service. Construction uses 0.9 ConnectorOptions from ServerConf. |
| Cache metadata extensions | `pingora-cache/src/{meta.rs,lib.rs}` | Keep transient Edge cache policy extensions and repeated revalidation header values; persisted metadata remains the current baseline, with no old-format decoder. |
| Bounded cache accounting | `tinyufo/src/lib.rs`, `pingora-memory-cache/src/{lib.rs,read_through.rs}`, `pingora-cache/src/memory.rs` | Expose memory item/weight statistics and borrowed-key lookup used by Edge's memory governor. |
| HTTP module inspection | `pingora-core/src/modules/http/mod.rs` | Ensure sendfile is selected only when it cannot bypass downstream body processing. |
| TLS provider/certificate identity | `pingora-rustls/{Cargo.toml,src/lib.rs}`, `pingora-core/Cargo.toml` | Preserve Edge's explicit AWS-LC provider, native-certificate API, and certificate SHA-256 identity. Provider selection does not rely on implicit resolution when multiple crypto crates are linked. |
| Default rustls handshake offload | `pingora-core/src/listeners/tls/rustls/mod.rs` | Preserve the existing blocking-pool offload for dynamic in-memory acceptors. Explicitly configured 0.9 dedicated offload pools still use the upstream implementation. |
| Shared H2 malformed-stream budget | `pingora-core/src/protocols/http/v2/server.rs` | Expose the existing 0.9 budgeted acceptance method to Edge's custom accept loop. The counter is per connection; rejected streams are observed without resetting the rejection budget on every accept. |
| Transformed HTTP/1 response framing | `pingora-proxy/src/proxy_h1.rs` | Frame the final response after response_filter, which may remove Content-Length during HLS/WebP/minification transformations. Preserve upstream upgrade validation. |

Existing extension regression tests are retained. Test-only adaptations account
for h2 0.4.20 rejecting duplicate Content-Length before Pingora acceptance: the
wire-level test still requires PROTOCOL_ERROR and no application session; the
Pingora malformed-budget test uses authority mismatch so it reaches that layer.
The connector test requires Darwin's exact InternalError -> BindError ->
EADDRNOTAVAIL chain for a loopback-bound reserved destination; non-macOS
assertions remain unchanged. No production error mapping is changed.
Upstream 0.9 implementations
replace the old connection-pool cleanup, shared runtime, tracing switches,
compression support, and related fixes where they already cover the behavior.
The old Prometheus-in-core and connector-local offload files are removed rather
than maintained as alternative implementations.

## Verification

See `../docs/orbstack-build-tests.md` for local-only validation. Linux builds
must embed a freshly built, hashed eBPF object. A macOS check does not validate
Linux XDP/AF_XDP paths or the x86-64-v2 CPU baseline.

To inspect this fork, compare this directory to the exact upstream commit above;
do not compare to upstream main, which can contain newer unreleased changes.
