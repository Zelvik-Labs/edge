#!/usr/bin/env bash
# 安装器配置写入路径与布局的回归测试。
#
# 必须在 root 下跑真正的写入路径才能覆盖到：脚本里有 `[ "$(id -u)" -eq 0 ]`
# 守卫，--dry-run 又会跳过写入。所以这里在一次性容器里以 root 执行 install.sh。
# 26.10.3.1 起 --install-dir 就是组件 home（/opt/zelvik/edge），不再有 staging
# 前缀；单元固定写 /etc/systemd/system，wrapper 固定写 /usr/local/bin。容器里
# 用假的 systemctl/journalctl 模拟启动与注册，只验证安装器的写盘与校验逻辑。
#
# 覆盖：
#   1. 二进制落在 <home>/bin，单元 ExecStart/Environment/WorkingDirectory 指向
#      同一 home；wrapper 导出 ZELVIK_EDGE_HOME。
#   2. 包内没有 data/*.o 时 <home>/data 仍归服务用户（历史 EACCES 重启根因）。
#   3. 重跑安装器整文件覆盖 api_node.yaml 会丢掉运维键；现在只重写自己的键。
#   4. --endpoint 逗号列表完整、按序落盘。
#   5. --dir 被删除且报错指向 --install-dir。
#   6. 已装节点不指定 --install-dir 时沿用旧 home；显式换目录被拒绝。
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
# install.sh 需要一个待安装的二进制；内容无关，只要存在、可执行、能报版本。
printf '#!/bin/sh\n[ "$1" = "--version" ] && echo "zelvik-edge 26.10.3.1" || true\n' > "$W/zelvik-edge"
chmod 0755 "$W/zelvik-edge"
printf '26.10.3.1\n' > "$W/VERSION"

# 容器没有 systemd：用假命令覆盖安装后的启动/注册校验，避免依赖真实 PID1。
FAKEBIN=/fakebin; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/systemctl" <<'EOS'
#!/bin/sh
case "$1" in
  is-active) [ "$#" -ge 3 ] && [ "$3" = "--quiet" ] && exit 0; echo active ;;
  show) echo 4242 ;;
  *) exit 0 ;;
esac
EOS
cat > "$FAKEBIN/journalctl" <<'EOS'
#!/bin/sh
echo 'edge instance registered on the control channel'
EOS
chmod 0755 "$FAKEBIN/systemctl" "$FAKEBIN/journalctl"
export PATH="$FAKEBIN:$PATH"

HOME_DIR=/opt/zelvik/edge
UNIT=/etc/systemd/system/zelvik-edge.service
CFG="$HOME_DIR/configs/api_node.yaml"
WRAPPER=/usr/local/bin/zelvik-edge

bash "$W/install.sh" --install-dir "$HOME_DIR" \
    --endpoint 'https://cp1.example.com,https://cp2.example.com,https://cp3.example.com' \
    --node-id nd_test --secret abc123 --comm-mode tls >/tmp/install1.log 2>&1 \
    || { tail -30 /tmp/install1.log; exit 1; }

fail=0
expect() { # expect <label> <file> <pattern>
    if grep -qF -- "$3" "$2"; then echo "OK   $1"; else echo "FAIL $1: $2 缺 $3"; fail=1; fi
}
absent() { # absent <label> <file> <pattern>
    if grep -qF -- "$3" "$2"; then echo "FAIL $1: $2 不该有 $3"; fail=1; else echo "OK   $1"; fi
}
owner() { # owner <label> <path> <expected user>
    local got; got="$(stat -c '%U:%a' "$2")"
    if [ "$got" = "$3" ]; then echo "OK   $1 ($got)"; else echo "FAIL $1: $2 owner=$got want $3"; fail=1; fi
}
endpoints() { # endpoints <file> — emit rpc.endpoints list entries in file order
    awk '
        /^rpc\.endpoints:[[:space:]]*$/ { in_list = 1; next }
        /^[^[:space:]]/ { in_list = 0 }
        in_list && /^  - "/ {
            line = $0
            sub(/^  - "/, "", line)
            sub(/"$/, "", line)
            print line
        }
    ' "$1"
}
expect_endpoints() { # expect_endpoints <label> <file> <comma-separated expected>
    local label="$1" file="$2" expected="$3" actual
    actual="$(endpoints "$file" | paste -sd, -)"
    if [ "$actual" = "$expected" ]; then
        echo "OK   $label"
    else
        echo "FAIL $label: expected [$expected], got [$actual]"
        fail=1
    fi
}

# 1) 布局：二进制落在 <home>/bin，单元三者一致，wrapper 导出 home。
[ -x "$HOME_DIR/bin/zelvik-edge" ] && echo "OK   二进制落在 <home>/bin" || { echo "FAIL 二进制未安装到 <home>/bin"; fail=1; }
expect "单元 ExecStart 指向 <home>/bin/zelvik-edge" "$UNIT" "ExecStart=$HOME_DIR/bin/zelvik-edge"
expect "单元 Environment 指向同一 home" "$UNIT" "Environment=ZELVIK_EDGE_HOME=$HOME_DIR"
expect "单元 WorkingDirectory 指向同一 home" "$UNIT" "WorkingDirectory=$HOME_DIR"
expect "wrapper 导出 ZELVIK_EDGE_HOME" "$WRAPPER" "export ZELVIK_EDGE_HOME=$HOME_DIR"
expect "wrapper exec 同一二进制" "$WRAPPER" "exec $HOME_DIR/bin/zelvik-edge"

# 2) 无 data/*.o 的包也必须把 data 交给服务用户；configs/bin/backups 归 root。
owner "data 归服务用户" "$HOME_DIR/data" 'zelvik-edge:750'
owner "logs 归服务用户" "$HOME_DIR/logs" 'zelvik-edge:750'
owner "bin 归 root" "$HOME_DIR/bin" 'root:755'
owner "configs 归 root" "$HOME_DIR/configs" 'root:750'
owner "backups 归 root" "$HOME_DIR/backups" 'root:700'
owner "api_node.yaml 0640 root:zelvik-edge" "$CFG" 'root:640'

# 3) 首装写出安装器拥有的键，且 xdp 默认关闭。
for k in 'rpc.endpoints:' 'instanceId: "nd_test"' 'secret: "abc123"' 'commMode: "tls"' 'enabled: false'; do
    expect "首装含 $k" "$CFG" "$k"
done
expect_endpoints "首装保留完整有序 endpoint 列表" "$CFG" \
    'https://cp1.example.com,https://cp2.example.com,https://cp3.example.com'

# 4) 运维改动后重跑：拥有的键更新，运维的键原样保留。
sed -i 's/^xdp:$/xdp:\n  enabled: true/' "$CFG"
{ printf 'kernelTuning:\n  rmemMax: 16777216\n'; printf 'gcloudNodeId: "1001"\n'; } >> "$CFG"

bash "$W/install.sh" \
    --endpoint 'https://cp4.example.com,https://cp5.example.com' \
    --node-id nd_test2 --secret def456 --comm-mode tls >/tmp/install2.log 2>&1 \
    || { tail -30 /tmp/install2.log; exit 1; }

expect_endpoints "重跑替换为新的完整有序 endpoint 列表" "$CFG" \
    'https://cp4.example.com,https://cp5.example.com'
absent "重跑移除旧 endpoint 1"  "$CFG" '"https://cp1.example.com"'
absent "重跑移除旧 endpoint 2"  "$CFG" '"https://cp2.example.com"'
absent "重跑移除旧 endpoint 3"  "$CFG" '"https://cp3.example.com"'
expect "重跑更新 instanceId"    "$CFG" 'instanceId: "nd_test2"'
expect "重跑更新 secret"        "$CFG" 'secret: "def456"'
absent "重跑不保留旧 secret"    "$CFG" 'secret: "abc123"'
expect "运维的 xdp 保留"        "$CFG" 'enabled: true'
expect "运维的 kernelTuning 保留" "$CFG" 'rmemMax: 16777216'
expect "运维的 gcloudNodeId 保留" "$CFG" 'gcloudNodeId: "1001"'
if [ "$(grep -c '^xdp:' "$CFG")" -eq 1 ]; then echo "OK   xdp 只出现一次"; else echo "FAIL xdp 重复"; fail=1; fi
if [ "$(grep -c '^# Rendered by deploy/install.sh' "$CFG")" -eq 1 ]; then echo "OK   表头只出现一次"; else echo "FAIL 表头堆叠"; fail=1; fi

# 5) 未指定 --install-dir 时沿用已装 home；显式换目录必须拒绝。
bash "$W/install.sh" --endpoint 'https://cp4.example.com' \
    --node-id nd_test2 --secret def456 --comm-mode tls >/tmp/install3.log 2>&1 \
    || { tail -30 /tmp/install3.log; exit 1; }
expect "重跑沿用已探测 home" "$UNIT" "WorkingDirectory=$HOME_DIR"
if bash "$W/install.sh" --install-dir /srv/other --endpoint 'https://cp4.example.com' \
    --node-id nd_test2 --secret def456 --comm-mode tls >/tmp/install4.log 2>&1; then
    echo "FAIL 显式切换已装 home 未被拒绝"; fail=1
else
    if grep -q 'refusing to switch' /tmp/install4.log; then echo "OK   显式换目录被拒绝"; else echo "FAIL 拒绝信息缺失"; tail -5 /tmp/install4.log; fail=1; fi
fi

# 6) 已删除的 --dir 必须报错并指向 --install-dir。
if bash "$W/install.sh" --dir /tmp/staging --endpoint 'https://cp4.example.com' \
    --node-id nd_test2 --secret def456 >/tmp/install5.log 2>&1; then
    echo "FAIL --dir 未被拒绝"; fail=1
else
    if grep -q 'Use --install-dir' /tmp/install5.log; then echo "OK   --dir 被拒绝并指向 --install-dir"; else echo "FAIL --dir 报错信息缺失"; tail -5 /tmp/install5.log; fail=1; fi
fi

exit "$fail"
INNER

docker run --rm \
    -v "$ROOT/deploy:/src:ro" \
    -v "$WORK/inner.sh:/inner.sh:ro" \
    "$IMAGE" bash /inner.sh