#!/usr/bin/env bash
# 清理本次构建的 target；不删除其他项目的共享构建缓存。
set -euo pipefail
cd "$(dirname "$0")/.."
python3 - <<'PY'
import os
import shutil
from pathlib import Path

target = Path(os.environ.get("CARGO_TARGET_DIR", "target")).resolve()
repo = Path.cwd().resolve()
if target in (Path("/"), Path.home(), repo) or repo.is_relative_to(target):
    raise SystemExit(f"refusing unsafe build target: {target}")
if target.exists():
    shutil.rmtree(target)
print(f"cleaned: {target}")
PY
