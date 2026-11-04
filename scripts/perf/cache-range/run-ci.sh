#!/usr/bin/env bash
set -euo pipefail
root=$(pwd)
out="$root/.local/cache-range-private"
mkdir -p "$out/tls"
curl --version > "$out/curl-version.txt"
dpkg-query -W > "$out/packages.txt"
uname -a > "$out/kernel.txt"
cc -O3 -march=x86-64-v2 -Wall -Wextra -Werror benches/support/protocol_state_client.c $(pkg-config --cflags --libs libcurl) -o "$out/protocol-state-client"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$out/tls/ca.key" -out "$out/tls/ca.pem" -days 2 -subj /CN=Edge-Range-CA > "$out/tls/build.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$out/tls/server.key" -out "$out/tls/server.csr" -subj /CN=localhost >> "$out/tls/build.log" 2>&1
cat > "$out/tls/server.ext" <<'CERT'
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
CERT
openssl x509 -req -in "$out/tls/server.csr" -CA "$out/tls/ca.pem" -CAkey "$out/tls/ca.key" -CAcreateserial -out "$out/tls/server.pem" -days 2 -extfile "$out/tls/server.ext" >> "$out/tls/build.log" 2>&1
python3 -m unittest discover -s scripts/perf -p 'test_*protocol*.py' > "$out/python-tests.log" 2>&1
python3 scripts/perf/cache-range/run.py
