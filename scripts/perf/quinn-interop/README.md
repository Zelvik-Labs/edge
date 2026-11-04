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

The first GnuTLS batches were too slow to complete and remain failed; 18,300
retained partial records were valid, with no claim about unwritten output.
The next control narrows both clients to the fixture CA via an explicit
`--fixture-ca-only` option, clears the system CA directory, and retains full
peer/hostname verification. A wrong-CA request must fail with curl 60 per case.
Request count and deadlines stay unchanged. This isolates trust-directory
cost without declaring the earlier incomplete batches successful.

Manual `groups` mode uses the same diagnostic OpenSSL executable for default
versus X25519-supported groups. Group selection comes from bounded ServerHello
key_share metadata parsing, not a recursive SSL getter in a callback. Slot
summaries prove observed group IDs; X25519 cases require ID 29. The parser is
metadata-only and does not validate cryptographic key material. This does not
change production crypto policy. Both modes are dispatched explicitly to avoid
rerunning a different experiment on documentation or shared harness changes.

Manual `pacing` mode keeps one diagnostic OpenSSL client and the original
server/controller. Both variants traverse the same bounded IPv4 loopback
relay, with server-to-client spacing0/100us in balanced order. The relay
is pinned to the formal HIT fixture's origin core; thread CPU, real minimum
send interval, queue peaks and shutdown leftovers are retained. Actual
client internal batching remains unknown. No relay/perf numbers are used
as server capacity. All16384 attempts/case and wrong-CA controls remain.
If the unpaced relay cannot reproduce the error, paced success does not
establish a fix. The relay forwards bytes unchanged and retains no payloads
in public metadata; bounds and I/O failures remain explicit failures.
