#!/usr/bin/env bash
# 安装器配置写入路径的回归测试。
#
# 必须在 root 下跑真正的写入路径才能覆盖到：脚本里有 `[ "$(id -u)" -eq 0 ]`
# 守卫，--dry-run 又会跳过写入。所以这里在一次性容器里以 root 执行 install.sh，
# 用一个临时的 --dir 前缀，不碰宿主机的 /usr/bin 与 /etc/systemd/system。
#
# 覆盖两条曾经的缺陷：
#   1. --dir 装完服务起不来：二进制落到 <dir>/usr/bin，而单元的 ExecStart 写死
#      /usr/bin/zelvik-edge。现在单元是模板，@ZELVIK_EDGE_BIN@ 由安装器替换。
#   2. 重跑安装器整文件覆盖 api_node.yaml，把运维设的 xdp / kernelTuning /
#      gcloudNodeId 等键丢掉。现在只重写安装器自己拥有的键。
#
# 用法：bash scripts/test-installer-config.sh
# 需要 docker；没有 docker 时跳过（不伪造通过）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-debian:stable-slim}"

if ! command -v docker >/dev/null 2>&1; then
    echo "SKIP docker not available: install.sh 的写入路径需要 root，本机无法直接覆盖"
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/inner.sh" <<'INNER'
set -eu
W=/work
rm -rf "$W"; mkdir -p "$W"
cp -r /src/. "$W/"
# install.sh 需要一个待安装的二进制；内容无关，只要存在且可执行。
printf '#!/bin/sh\necho dummy\n' > "$W/zelvik-edge"
chmod 0755 "$W/zelvik-edge"
mkdir -p "$W/data"

DIR=/opt/zelvik
UNIT="$DIR/etc/systemd/system/zelvik-edge.service"
CFG="$DIR/var/lib/zelvik/edge/configs/api_node.yaml"

bash "$W/install.sh" --dir "$DIR" --endpoint https://cp.example.com \
    --node-id nd_test --secret abc123 --comm-mode tls >/tmp/install1.log 2>&1 \
    || { tail -20 /tmp/install1.log; exit 1; }

fail=0
expect() { # expect <label> <file> <pattern>
    if grep -qF -- "$3" "$2"; then echo "OK   $1"; else echo "FAIL $1: $2 缺 $3"; fail=1; fi
}
absent() { # absent <label> <file> <pattern>
    if grep -qF -- "$3" "$2"; then echo "FAIL $1: $2 不该有 $3"; fail=1; else echo "OK   $1"; fi
}

# 1) --dir：二进制落点、单元 ExecStart、建目录三者必须一致
[ -x "$DIR/usr/bin/zelvik-edge" ] && echo "OK   二进制落在 $DIR/usr/bin" || { echo "FAIL 二进制未安装到 --dir 前缀"; fail=1; }
expect "单元 ExecStart 指向实际二进制"  "$UNIT" "ExecStart=$DIR/usr/bin/zelvik-edge"
absent "单元不含写死的 /usr/bin/zelvik-edge" "$UNIT" "ExecStart=/usr/bin/zelvik-edge"

# 2) 首装写出安装器拥有的键，且 xdp 默认关闭
for k in 'rpc.endpoints:' 'instanceId: "nd_test"' 'secret: "abc123"' 'commMode: "tls"' 'enabled: false'; do
    expect "首装含 $k" "$CFG" "$k"
done

# 3) 运维改动后重跑：拥有的键更新，运维的键原样保留
sed -i 's/^xdp:$/xdp:\n  enabled: true/' "$CFG"
{ printf 'kernelTuning:\n  rmemMax: 16777216\n'; printf 'gcloudNodeId: "1001"\n'; } >> "$CFG"

bash "$W/install.sh" --dir "$DIR" --endpoint https://cp2.example.com \
    --node-id nd_test2 --secret def456 --comm-mode tls >/tmp/install2.log 2>&1 \
    || { tail -20 /tmp/install2.log; exit 1; }

expect "重跑更新 endpoint"      "$CFG" '"https://cp2.example.com"'
expect "重跑更新 instanceId"    "$CFG" 'instanceId: "nd_test2"'
expect "重跑更新 secret"        "$CFG" 'secret: "def456"'
absent "重跑不保留旧 secret"    "$CFG" 'secret: "abc123"'
expect "运维的 xdp 保留"        "$CFG" 'enabled: true'
expect "运维的 kernelTuning 保留" "$CFG" 'rmemMax: 16777216'
expect "运维的 gcloudNodeId 保留" "$CFG" 'gcloudNodeId: "1001"'
# 运维已设 xdp 时不得再补一份默认，否则 YAML 出现重复键
if [ "$(grep -c '^xdp:' "$CFG")" -eq 1 ]; then echo "OK   xdp 只出现一次"; else echo "FAIL xdp 重复"; fail=1; fi
# 安装器自己的表头不得随重跑堆叠
if [ "$(grep -c '^# Rendered by deploy/install.sh' "$CFG")" -eq 1 ]; then echo "OK   表头只出现一次"; else echo "FAIL 表头堆叠"; fail=1; fi

exit "$fail"
INNER

docker run --rm \
    -v "$ROOT/deploy:/src:ro" \
    -v "$WORK/inner.sh:/inner.sh:ro" \
    "$IMAGE" bash /inner.sh
