# Matched Range preflight comparison

The production change defers full-object metadata and header validation used
only by Range selection. Ordinary cache requests keep storage-layer validation.
The cache partial-store unit tests require zero metadata probes without Range,
and preserve 32 combinations of range syntax, full-object availability and
partial-cache policy.

Actions builds baseline `a4e95a239abb82420f65d79f813654ba67fedad9` and the
candidate sequentially in the same checkout and toolchain, then copies both
actual executables with SHA-256. The only production source differences must
be `src/proxy.rs` and `src/cache/partial.rs`. No measurements run until both
builds and all image preparation finish. This script checks out immutable
commits only in the isolated CI job; local builds remain subject to mbx rules.

One Williams cycle balances four target positions and all directed predecessor
pairs: before, after, Nginx 1.28.0, OpenResty 1.27.1.2. Each target runs alone,
with a fresh process and tmpfs per case. H1, H1 TLS, H2 and H3 each exercise
HIT/OFF/MISS with 1 KiB bodies, 32 reuse connections and five-second windows.
Unique MISS URLs run for the same time window, rather than a small fixed count.
The total is 192 cells and four matched before/after pairs per condition.
This targeted comparison does not rank fresh connections or H2/H3 multiplexing;
the earlier complete protocol matrix retains those cases separately.

Before measuring a warmed HIT, two real Range controls check 206, protocol,
TLS verification and exact prefix/suffix bytes. They run before resource and
origin counters are sampled. If the full object is not seekable, an existing
bypass may reach origin; the ordinary warmed HIT is verified again afterward.
All 128 Range controls are required when all 192 cells pass. Case failures are
retained and later targets continue; failed or incomplete cells get no median.

The verifier recomputes every transfer, origin/cache contract, CPU/request,
P99 and matched percentage changes. The proxy cgroup also includes the test
controller; its peak is not Edge RSS. Inspect client/origin CPU and steal
before interpreting results. Four short repetitions do not establish a long
soak or performance on real NICs.

The public artifact uses an explicit allow-list: no runtime overrides with TLS
keys, private TLS material, node logs or controller stores. Actual measured
executables are retained separately. Sources, image identities, masks, flags,
raw transfers, Range responses and failures remain inspectable.

```sh
python3 scripts/perf/cache-range/verify.py /path/to/extracted-evidence \
  /path/to/extracted-binaries/.local/cache-range-binaries
```
