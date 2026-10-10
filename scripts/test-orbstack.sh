#!/usr/bin/env bash
# Local compilation/tests only. Never touches orb-deploy or publishes artifacts.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-arm64}"
PHASE="${2:-all}"
case "$ARCH" in arm64|amd64) ;; *) printf 'usage: %s [arm64|amd64] [all|check|test|clippy|pingora]\n' "$0" >&2; exit 2 ;; esac
case "$PHASE" in all|check|test|clippy|pingora) ;; *) printf 'unknown phase: %s\n' "$PHASE" >&2; exit 2 ;; esac

IMAGE="zelvik-edge-orb-test:1.99.0-$ARCH"
RESULTS="${ZELVIK_EDGE_TEST_RESULTS:-$ROOT/../artifacts/edge-orb-tests/$(date +%Y%m%d-%H%M%S)-$ARCH}"
mkdir -p "$RESULTS"
RESULTS="$(cd "$RESULTS" && pwd)"
CONTEXT="$(mktemp -d)"
trap 'rmdir "$CONTEXT"' EXIT

docker --context orbstack build --platform "linux/$ARCH" \
    -f "$ROOT/Dockerfile.orb-test" -t "$IMAGE" "$CONTEXT" \
    2>&1 | tee "$RESULTS/image-build.log"

# Architecture-specific volumes isolate native libraries and Rust toolchains.
# No host Cargo/mbx cache is mounted into Linux.
docker --context orbstack run --rm --platform "linux/$ARCH" \
    --name "zelvik-edge-orb-test-$ARCH" \
    --cpus "${ZELVIK_EDGE_BUILD_CPUS:-2}" --memory "${ZELVIK_EDGE_BUILD_MEMORY:-4g}" \
    --mount "type=bind,source=$ROOT,target=/source,readonly" \
    --mount "type=bind,source=$RESULTS,target=/results" \
    --mount "type=volume,source=zelvik-edge-test-workspace-$ARCH,target=/workspace" \
    --mount "type=volume,source=zelvik-edge-test-registry-$ARCH,target=/usr/local/cargo/registry" \
    --mount "type=volume,source=zelvik-edge-test-rustup-$ARCH,target=/usr/local/rustup" \
    --mount "type=volume,source=zelvik-edge-test-tools-$ARCH,target=/tooling" \
    --mount "type=volume,source=zelvik-edge-test-mbx-$ARCH,target=/var/cache/zelvik-mbx" \
    -e "ZELVIK_EDGE_TEST_PHASE=$PHASE" -e "ZELVIK_EDGE_TEST_ARCH=$ARCH" \
    -e "SOURCE_DATE_EPOCH=$(git -C "$ROOT" log -1 --format=%ct)" \
    "$IMAGE" bash /source/scripts/test-orbstack-container.sh \
    2>&1 | tee "$RESULTS/container.log"
printf 'Local test results: %s\n' "$RESULTS"
