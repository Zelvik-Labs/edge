# H3 origin repeated-header evidence

Source `b5bc67ee7484915d7d6f86319cf239ff5d8ade38`, Actions [41629076252](https://github.com/Zelvik-Labs/edge/actions/runs/41629076252). Original artifact 13228395386 is retained with its public metadata and every-file hashes. Eight named tests pass in the retained log excerpt: seven real H3 tests and one conversion regression. Successful normal responses now assert both cookies and repeated custom values in source.

```bash
python3 unpack.py
python3 validate-wire.py wire wire-validation.json b5bc67ee7484915d7d6f86319cf239ff5d8ade38 41629076252
```

Counts are 21 resolved requests including six abnormal/aborted/timeout controls, ten accepted/closed QUIC connections, and twelve sequential requests on one QUIC connection/driver. These are Kernel-loopback correctness records, not throughput, physical-NIC or long-soak evidence. No private download URLs are retained.
