#!/usr/bin/env bash
# 构建产物清理：每次构建后运行，防止本地磁盘被中间产物占满。
# 覆盖：本仓 target/、mbx 共享构建缓存（外部化 target）、Cargo 注册表增量。
set -euo pipefail
cd "$(dirname "$0")/.."

rm -rf target
# mbx 把 target 外部化到 ~/Library/Caches/mbx —— 这才是真正的磁盘大头。
if [ -d "$HOME/Library/Caches/mbx" ]; then
  rm -rf "$HOME/Library/Caches/mbx"/*
fi
echo "cleaned: target/ + ~/Library/Caches/mbx"
