#!/usr/bin/env bash
# Runs inside the local fixture container; no target measurements overlap.
set -euo pipefail
root=$(pwd)
out="$root/.local/http-state-ci"
mkdir -p "$out/tls"
curl --version > "$out/curl-version.txt"
rg_http3=$(curl --version)
[[ "$rg_http3" == *HTTP3* ]] || { echo 'libcurl must support real HTTP/3' >&2; exit 1; }
dpkg-query -W > "$out/packages.txt"
cc -O3 -march=x86-64-v2 -Wall -Wextra -Werror benches/support/protocol_state_client.c $(pkg-config --cflags --libs libcurl) -o "$out/protocol-state-client"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$out/tls/ca.key" -out "$out/tls/ca.pem" -days 2 -subj /CN=Edge-Matrix-CA > "$out/tls/build.log" 2>&1
openssl req -newkey rsa:2048 -nodes -keyout "$out/tls/server.key" -out "$out/tls/server.csr" -subj /CN=localhost >> "$out/tls/build.log" 2>&1
cat > "$out/tls/server.ext" <<'CERT'
subjectAltName=DNS:localhost,IP:127.0.0.1
extendedKeyUsage=serverAuth
CERT
openssl x509 -req -in "$out/tls/server.csr" -CA "$out/tls/ca.pem" -CAkey "$out/tls/ca.key" -CAcreateserial -out "$out/tls/server.pem" -days 2 -extfile "$out/tls/server.ext" >> "$out/tls/build.log" 2>&1
python3 -m unittest discover -s scripts/perf -p 'test_*protocol*.py' > "$out/python-tests.log" 2>&1
case "${EDGE_HTTP_SUITE:-comparison}" in
  comparison) python3 scripts/perf/http-state/run.py ;;
  native) python3 scripts/perf/h3-native/run.py ;;
  native-admission)
    python3 scripts/perf/protocol_state_matrix.py \
      --output "$out/admission-quiche" --target edge --tls "$out/tls" \
      --client "$out/protocol-state-client" --edge-binary "$root/target/release/Zelvik-Edge" \
      --controller "$root/target/release/examples/issue34-controller" \
      --edge-image edge-http-state-fixture:20261103 --edge-backend tokio-quiche-native \
      --transport-profile matched --address-validation off --protocols h3 \
      --states hit --connections fresh --repeats 3 --fresh-requests 512 \
      --payload-size 1024 --concurrency 32
    ;;
  *) echo "unknown EDGE_HTTP_SUITE" >&2; exit 1 ;;
esac
