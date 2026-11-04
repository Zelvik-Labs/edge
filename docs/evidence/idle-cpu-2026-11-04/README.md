# Idle CPU evidence

The original, unmodified Actions artifact 13318731608 comes from run
[41823827416](https://github.com/Zelvik-Labs/edge/actions/runs/41823827416).
`artifact.json` records GitHub's digest and source, and `manifest.json` records
every original ZIP entry. The source scripts and generated records/summary are
also retained for inspection. See the [report](../../performance-idle-cpu-2026-11-04.md).

`input-binaries/` records the original header-node binary artifact 13290468969
(source 4171ea700386d8ed7c755255c7eeccb7e804d644, 38,769,957 bytes, SHA-256
`7d41425305dd0456ce9116a138e19217eb8a063bdad43b047437464bf4d84728`).
Original ZIP bytes are published as version attachments
[`evidence-idle-cpu-41823827416.zip`](https://github.com/Zelvik-Labs/edge/releases/download/v26.11.1.1/evidence-idle-cpu-41823827416.zip)
and [`evidence-header-binaries-41757321970.zip`](https://github.com/Zelvik-Labs/edge/releases/download/v26.11.1.1/evidence-header-binaries-41757321970.zip).
Save each as `raw.zip` in a separate download directory with the matching
metadata from `pass/` or `input-binaries/`, and extract it to `extracted/`.
Replay requires full Git history including the
preserved `perf/program-wide-20261104` branch. Each input directory must contain
`raw.zip`, its original `artifact.json`, its extraction `manifest.json`, and
an `extracted/` directory with all original entries. No measured executable runs.

```bash
python3 docs/evidence/idle-cpu-2026-11-04/pass/verify.py IDLE_DOWNLOAD HEADER_BINARY_DOWNLOAD
```

The verifier checks both ZIP digests, every extracted byte, original source
identity, actual measured input executable digests, all HTTP transfer records,
and all CPU snapshots. It regenerates records and summary in a temporary copy
and requires exact equality with the original results. CPU is never adjusted by
subtracting an idle reference. The 8 complete cells contain 587,900 successful
responses and zero errors; this is a short diagnostic using historical binaries,
not a latest-release capacity or long-soak claim.
