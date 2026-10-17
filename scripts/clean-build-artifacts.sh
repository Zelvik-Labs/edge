#!/usr/bin/env bash
# Disk-reclamation tool, not a per-build routine: run only when disk is tight, and only
# after the workspace's build has exited. `target/` is the Rust build cache — deleting it
# every round turns each iteration into a cold build (see AGENTS.md). Keep shared mbx objects.
set -euo pipefail
exec python3 -I "$(dirname "$0")/clean-build-artifacts.py" "$@"
