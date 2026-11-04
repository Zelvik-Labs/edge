# Shared kernel TX candidate

Run 41762751769; candidate source 27a47d67d54c95087af7c2f24ef0cc844a0e192e.
Artifact 13291637592 retains the actual probe and copied source/lockfile.
No production integration or throughput result. Initial registration readiness
is genuine; actual send-buffer exhaustion is not covered.

```sh
python3 docs/evidence/shared-quic-tx-2026-11-04/verify.py DOWNLOAD
```

DOWNLOAD contains raw.zip, artifact.json, manifest.json and extracted/.
The verifier checks every archive byte, actual executable/source hashes and
IPv4/IPv6 segmentation, GRO and hard-error controls.
