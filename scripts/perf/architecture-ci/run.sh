#!/usr/bin/env bash
# Diagnose the exact already-measured production binary; no compilation here.
set -euo pipefail
root=$(pwd)
out="$root/.local/architecture-profile-private"
bins="$root/.local/architecture-profile-binaries"
mkdir -p "$out/tls"
python3 scripts/perf/architecture-ci/evidence.py prepare
curl --version > "$out/curl-version.txt"
perf --version > "$out/perf-version.txt"
uname -a > "$out/kernel.txt"
dpkg-query -W > "$out/packages.txt"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$out/tls/ca.key" -out "$out/tls/ca.pem" -days 2 -subj /CN=Edge-Profile-CA > "$out/tls/build.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$out/tls/server.key" -out "$out/tls/server.csr" -subj /CN=localhost >> "$out/tls/build.log" 2>&1
cat > "$out/tls/server.ext" <<'CERT'
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
CERT
openssl x509 -req -in "$out/tls/server.csr" -CA "$out/tls/ca.pem" -CAkey "$out/tls/ca.key" -CAcreateserial -out "$out/tls/server.pem" -days 2 -extfile "$out/tls/server.ext" >> "$out/tls/build.log" 2>&1
python3 scripts/perf/profile_architecture.py \
  --output "$out/profile" --tls "$out/tls" \
  --client "$bins/.local/http-state-ci/protocol-state-client" \
  --edge-binary "$bins/target/release/Zelvik-Edge" \
  --controller "$bins/target/release/examples/issue34-controller" \
  --profiler-image edge-architecture-profiler:20261104 \
  --protocols h1 h1-tls h2 --states hit off miss --repeats 2 \
  --seconds 20 --frequency 199 --timed-misses
python3 scripts/perf/architecture-ci/evidence.py verify
