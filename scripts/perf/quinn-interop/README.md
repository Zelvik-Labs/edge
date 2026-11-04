# Same-version client interoperability

The native Quinn diagnostic observed KEY_UPDATE_ERROR from the OpenSSL client.
This experiment keeps the exact original Edge/controller, client C source,
compiler flags, curl Debian package version, server transport, body and request
count constant. Only libcurl's QUIC/TLS backend changes: OpenSSL native QUIC
versus GnuTLS + ngtcp2. Both packaged libraries must have exactly the same
Debian version or setup fails before traffic. Neither client enables traces.

OpenSSL/GnuTLS/GnuTLS/OpenSSL order runs four independent 32KiB HIT cases with
16,384 fresh attempts each, matched transport, Retry off and GSO auto. Every
case gets a fresh production process and cache. Errors still fail the workflow.
The verifier recomputes protocol/body/connection records and validates actual
server/client binaries and all retained client shared-library hashes.

These are interoperability/error observations. Different client stacks must
not be ranked as server capacity. Zero failures would constrain the cause but
would not prove a fix or 24/72h stability. Runtime source is pinned at
`82e3cf0db722497fb594acdcfabc645a6deb2b56`; latest cache changes are not included.

Public evidence uses an explicit allow-list and excludes private test TLS
material, runtime overrides, node logs and stores. Actual executables and their
client libraries are retained in a separate artifact. No upstream/vendor code
or production default is changed by this experiment.
