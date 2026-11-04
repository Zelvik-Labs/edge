# Same OpenSSL binary, default versus X25519

Run 41764056727 failed actual traffic: 14/32768 default and 8/32768 X25519 errors.
Both complete, 4 wrong-CA rejections; the same 22 local KEY_UPDATE_ERROR closes.
Selected group metadata covers all 65,536 attempts, but does not prove packet
authentication. Group selection alone does not eliminate the fault.

Download evidence 13291664074 and binaries/libraries 13292211072 with each
raw.zip, artifact.json, manifest.json and extracted/ retained, then run:

```sh
python3 docs/evidence/quinn-interop-2026-11-04/verify.py EVIDENCE BINARIES --groups
```

It checks all archive bytes, actual binaries/libraries, transfers, slot group
observations, negative CA controls and local close patterns. Public files use
an explicit allow-list; no private test TLS, overrides or stores are copied.
