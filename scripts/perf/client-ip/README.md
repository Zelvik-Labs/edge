# Small HeaderMap IP fallback candidate

This is an exact-IP-parser, read-only HeaderMap component, not Pingora I/O
or production throughput. Only private/local peers take the fallback path.
At most four fields use a priority-aware scan after the original first two
priority lookups; larger maps retain the exact original loop. Duplicate
headers use the first value only, even when later duplicates parse.

The immutable c8df43c baseline and all IP parsing helpers are extracted
verbatim.75,923 complete differential rows cover12priority names, duplicate
valid/invalid/non-ASCII values, six peer forms, and deterministic generated
maps.19 separate timing cases include public peers and large maps. Eight
alternating pairs,20blocks/case, pinned CPU, steal tracking, separate zero
allocation verification and strict Clippy are all retained in Actions.

```sh
python3 scripts/perf/client-ip/run.py build
python3 scripts/perf/client-ip/run.py measure
python3 scripts/perf/client-ip/run.py verify --directory EXTRACTED
```

No production integration, trust-boundary change or performance claim yet.
