#!/usr/bin/env bash
# Unmodified, pinned upstream releases. Complete all builds before measurements.
set -euo pipefail
[[ $# == 1 ]] || { echo 'usage: build-baselines.sh absolute-output-dir' >&2; exit 2; }
bench_output=$(realpath -m "$1")
mkdir -p "$bench_output/downloads" "$bench_output/source"
fetch() {
  local name=$1 url=$2 digest=$3
  if [[ ! -f "$bench_output/downloads/$name" ]]; then
    curl --fail --show-error --silent --location --retry 3 "$url" -o "$bench_output/downloads/$name"
  fi
  echo "$digest  $bench_output/downloads/$name" | sha256sum -c -
}
fetch nginx-1.28.0.tar.gz https://nginx.org/download/nginx-1.28.0.tar.gz c6b5c6b086c0df9d3ca3ff5e084c1d0ef909e6038279c71c1c3e985f576ff76a
fetch openresty-1.27.1.2.tar.gz https://openresty.org/download/openresty-1.27.1.2.tar.gz 74f076f7e364b2a99a6c5f9bb531c27610c78985abe956b442b192a2295f7548
for target in nginx-1.28.0 openresty-1.27.1.2; do
  if [[ ! -d "$bench_output/source/$target" ]]; then
    tar -xzf "$bench_output/downloads/$target.tar.gz" -C "$bench_output/source"
  fi
done
(
  cd "$bench_output/source/nginx-1.28.0"
  ./configure --prefix="$bench_output/nginx" --with-stream --with-http_ssl_module --with-http_v2_module --with-cc-opt='-O3 -march=x86-64-v2'
  make -j2
  make install
) > "$bench_output/nginx-build.log" 2>&1
(
  cd "$bench_output/source/openresty-1.27.1.2"
  ./configure -j2 --prefix="$bench_output/openresty" --with-stream --with-http_ssl_module --with-http_v2_module --with-cc-opt='-O3 -march=x86-64-v2'
  make -j2
  make install
) > "$bench_output/openresty-build.log" 2>&1
"$bench_output/nginx/sbin/nginx" -V > "$bench_output/nginx-version.log" 2>&1
"$bench_output/openresty/nginx/sbin/nginx" -V > "$bench_output/openresty-version.log" 2>&1
