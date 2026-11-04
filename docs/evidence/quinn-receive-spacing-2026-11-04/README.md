# Immutable receive-spacing diagnostic

Run41769962134, source7df4a6e, runtime82e, same diagnostic OpenSSL client.
Unpaced20/32768 errors, paced1/32768, all4cases complete and4wrong-CA
rejections. All21errors are local KEY_UPDATE_ERROR; no production fix.
Relay send spacing does not prove client internal batching. Extra relay CPU
and timing changes exclude capacity ranking. Distinct UDP tuples are not
fresh QUIC connection counts because client source ports can be reused.

```sh
python3 docs/evidence/quinn-receive-spacing-2026-11-04/verify.py EVIDENCE BINARIES
```

Artifacts13294607034 and13293589334 use raw.zip/artifact.json/manifest.json/
extracted layouts. The frozen verifier checks all bytes, actual binaries and
libraries, every transfer, negative CA controls, both command shards, relay
affinity/counters/intervals/bounds, and local close patterns. Original network
failure and the first verifier filename failure remain in the run history.
Public files use explicit allow-lists; no TLS private material is included.
