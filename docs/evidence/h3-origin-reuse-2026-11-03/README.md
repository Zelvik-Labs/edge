# H3 origin reuse evidence

Frozen source: `28feef16bbb39b11d3701fbbac1959c608bfa9e5`. Actions run: [41623661117](https://github.com/Zelvik-Labs/edge/actions/runs/41623661117).

`wire/` and `native/` retain the original ZIP, public artifact metadata and SHA-256 of every original file. Raw logs stay unmodified in their archives. No temporary private download URLs are retained.

Run from this directory:

```bash
python3 unpack.py
python3 validate-wire.py wire wire-validation.json 28feef16bbb39b11d3701fbbac1959c608bfa9e5
python3 validate-native.py native/extracted native-validation.json 28feef16bbb39b11d3701fbbac1959c608bfa9e5
```

Unpacking is idempotent only when existing files remain byte-identical. The independent validators compare each real wire case and all 32 native logs with summary/metadata records. Seven wire cases resolve 21 requests, including six abnormal/aborted/timeout controls; this is not 21 successful responses. Twelve sequential requests reuse one QUIC connection and driver. Kernel-loopback/veth correctness is not throughput, real-NIC AF_XDP or long-soak evidence.
