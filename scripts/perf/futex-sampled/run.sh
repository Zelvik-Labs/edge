#!/usr/bin/env bash
set -euo pipefail
out="$PWD/.local/futex-sampled-private"
mkdir -p "$out/tls"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$out/tls/ca.key" -out "$out/tls/ca.pem" -days 2 -subj /CN=Edge-Profile-CA > "$out/tls/build.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$out/tls/server.key" -out "$out/tls/server.csr" -subj /CN=localhost >> "$out/tls/build.log" 2>&1
cat > "$out/tls/server.ext" <<'CERT'
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
CERT
openssl x509 -req -in "$out/tls/server.csr" -CA "$out/tls/ca.pem" -CAkey "$out/tls/ca.key" -CAcreateserial -out "$out/tls/server.pem" -days 2 -extfile "$out/tls/server.ext" >> "$out/tls/build.log" 2>&1
python3 scripts/perf/futex-sampled/evidence.py run
