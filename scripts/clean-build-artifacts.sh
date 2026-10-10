#!/usr/bin/env bash
# Run only after the workspace's build has exited; keep shared mbx objects.
set -euo pipefail
exec python3 -I "$(dirname "$0")/clean-build-artifacts.py" "$@"
