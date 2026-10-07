#!/usr/bin/env bash
set -euo pipefail

export LOCAL_UAM_KEY_LIFE="${LOCAL_UAM_KEY_LIFE:-3600}"
export LOCAL_UAM_POW_DIFFICULTY="${LOCAL_UAM_POW_DIFFICULTY:-5}"
export RUST_LOG="${RUST_LOG:-info,zelvik_edge::proxy=debug,zelvik_edge::http_proxy_manager=debug}"

exec cargo run --bin local-uam-lab
