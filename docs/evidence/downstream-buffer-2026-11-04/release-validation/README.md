# Release candidate correctness validation

Runtime candidate: `2c32f2462fcc9dd4bb77ff1372706fd2047f5f42`.
The measured buffer runtime is `60cf30ced5f85f68b84f27e170972ca528115434`;
subsequent production-path changes consist only of a test fixture correction in
`src/memory_governor.rs`. Version and independent product documentation are
prepared separately. Final report changes do not alter production code.

The final default CI
[41828592140](https://github.com/Zelvik-Labs/edge/actions/runs/41828592140)
passed project/Pingora checks, Clippy, 1,375 default library tests, 16 binary tests,
110 integration tests and 827 vendored Pingora tests. Five project performance
probes and two pre-existing Pingora probes remain explicitly ignored. All four
new TCP constructor regressions and the root socket-digest regression passed.
All 32 full-node copy-mode combinations passed and left only loopback with no
network namespaces; the binaries stayed unchanged throughout the combinations.
The actual PR merge ref is `f2ffc58225000761c2610979403c0458c090e704`; its tree and
the RC tree both equal `785ab5586a3274c5e72ac6a261a5309c2d315706`.

`native/` preserves the unmodified 137,472-byte artifact 13324317582, SHA-256
`a39d2851c2dbda292d83c59caf433f60997678e593fea4a45901d23cd60c9926`,
its API and extraction manifests and all 69 original files. Every byte and all
32 completion/cleanup records were checked independently. Structured gate
counts and identities are in [summary.json](summary.json).
Original files are retained inside `raw.zip`; extract to a scratch directory
for inspection, and use `manifest.json` to check each file's length and SHA-256.
The ZIP and logs are never normalized or rewritten.

The bounded state/durable recovery screen
[41828592225](https://github.com/Zelvik-Labs/edge/actions/runs/41828592225)
also passed. Manual 4-hour/48-hour and full-node long-soak jobs were not run.

The first RC CI [41825556333](https://github.com/Zelvik-Labs/edge/actions/runs/41825556333)
passed 1,374 library tests and failed the reserve test's assumption that rejecting
one HTTP connection necessarily leaves no room for a smaller stream. The extra
1,460-byte connection charge exposes this ordinary-budget remainder. Commit
2c32f24 fills smaller admissions up to their normal boundary, checks they never
enter the established reserve, and retains the stream/request rejection, reserved
origin admission, and complete release checks. Admission policy is unchanged.

The optional tokio-quiche feature run
[41830648938](https://github.com/Zelvik-Labs/edge/actions/runs/41830648938)
passed `cargo check --locked --features h3-quiche --all-targets`, the corresponding
Clippy correctness/suspicious gate, and library/binary tests: 1,383 library and
16 binary tests passed, zero failures, five explicit ignored probes. The
`h3-quiche/` directory preserves all three original logs and API/extraction
manifests; each log matches the original artifact's per-file digest. Artifact
13323721133: 119,670 bytes, SHA-256
`531e16f4676eb48f57d5f52c52d88dcce2992aeedc0047faad7b161b765badb2`.

This feature check does not replace a latest-node capacity comparison of the
experimental backend. Shared Quinn remains the production default. A successful
unit test run and bounded recovery screen do not establish 24/72-hour stability.
