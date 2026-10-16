#!/usr/bin/env bash
# Local compilation/tests only. Never touches orb-deploy or publishes artifacts.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${1:-arm64}"
PHASE="${2:-all}"
case "$ARCH" in arm64|amd64) ;; *) printf 'usage: %s [arm64|amd64] [all|check|test|clippy|pingora]\n' "$0" >&2; exit 2 ;; esac
case "$PHASE" in all|check|test|clippy|pingora) ;; *) printf 'unknown phase: %s\n' "$PHASE" >&2; exit 2 ;; esac

source "$ROOT/scripts/orbstack-protocol.sh"
# ZELVIK_EDGE_TEST_KEEP_CACHE=1 opts into keeping the tools/rustup volumes
# (bpf-linker and the dated nightly are rebuilt from scratch otherwise), with
# reuse keyed by ARCH. Everything else is still removed on every exit. Validated
# before anything is created so a bad value cannot leak a temp root.
KEEP_CACHE="${ZELVIK_EDGE_TEST_KEEP_CACHE:-0}"
case "$KEEP_CACHE" in 0|1) ;; *) printf 'ZELVIK_EDGE_TEST_KEEP_CACHE must be 0 or 1\n' >&2; exit 2 ;; esac

RESULTS="${ZELVIK_EDGE_TEST_RESULTS:-$ROOT/../artifacts/edge-orb-tests/$(date +%Y%m%d-%H%M%S)-$ARCH}"
mkdir -p "$RESULTS"
RESULTS="$(cd "$RESULTS" && pwd)"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/zelvik-edge-orb-test.XXXXXX")"
RUN_ID="${TEMP_ROOT##*.}"
IMAGE="zelvik-edge-orb-test:1.99.0-$ARCH-$RUN_ID"
CONTEXT="$TEMP_ROOT/build-context"
MIRROR="$TEMP_ROOT/protocol.git"
CIDFILE="$TEMP_ROOT/container.cid"
VOLUMES=()
MIRROR_ARGS=()
IMAGE_ATTEMPTED=0

cleanup() {
    local status=$? cleanup_status=0 cid remaining volume
    trap - EXIT INT TERM
    if [ -s "$CIDFILE" ]; then
        cid="$(<"$CIDFILE")"
        if remaining="$(docker --context orbstack container ls -aq --filter "id=$cid")"; then
            if [ -n "$remaining" ]; then
                docker --context orbstack rm -fv "$cid" >/dev/null || cleanup_status=$?
            fi
        else
            cleanup_status=$?
        fi
        if [ "$cleanup_status" -ne 0 ]; then
            printf 'Container cleanup failed for %s; retaining its bind at %s and resources for recovery.\n' "$cid" "$TEMP_ROOT" >&2
            if [ "$status" -ne 0 ]; then exit "$status"; fi
            exit "$cleanup_status"
        fi
    fi
    if [ "${#VOLUMES[@]}" -gt 0 ]; then
        for volume in "${VOLUMES[@]}"; do
            # Retained cache volumes outlive the run on purpose; every other
            # volume is removed even when an earlier removal failed.
            case ",$KEEP_CACHE:$volume" in
                ,1:zelvik-edge-test-tools-$ARCH | ,1:zelvik-edge-test-rustup-$ARCH)
                    printf 'Retaining cache volume %s (ZELVIK_EDGE_TEST_KEEP_CACHE=1).\n' "$volume" >&2
                    continue ;;
            esac
            docker --context orbstack volume rm "$volume" >/dev/null || cleanup_status=$?
        done
    fi
    if [ "$IMAGE_ATTEMPTED" -eq 1 ]; then
        if remaining="$(docker --context orbstack image ls -q "$IMAGE")"; then
            if [ -n "$remaining" ]; then
                docker --context orbstack image rm "$IMAGE" >/dev/null || cleanup_status=$?
            fi
        else
            cleanup_status=$?
        fi
    fi
    rm -rf -- "$TEMP_ROOT" || cleanup_status=$?
    if [ "$status" -ne 0 ]; then exit "$status"; fi
    exit "$cleanup_status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$CONTEXT"
# The pingora phase compiles only the independent pingora-main workspace: it has
# no private git dependency to resolve. One shared decision point with the
# container runner, so a new phase cannot be routed through (or away from) the
# private preflight in only one of them.
if phase_builds_main_workspace "$PHASE"; then
    # Authenticate and validate the exact locked revision before any image/toolchain build.
    prepare_protocol_mirror "$ROOT/Cargo.lock" "$MIRROR" \
        > "$RESULTS/protocol-fetch.log" 2>&1 || {
        printf 'Private dependency preflight failed; see %s/protocol-fetch.log\n' "$RESULTS" >&2
        exit 1
    }
    MIRROR_ARGS=(--mount "type=bind,source=$MIRROR,target=/protocol.git,readonly")
fi

IMAGE_ATTEMPTED=1
docker --context orbstack build --platform "linux/$ARCH" \
    -f "$ROOT/Dockerfile.orb-test" -t "$IMAGE" "$CONTEXT" \
    2>&1 | tee "$RESULTS/image-build.log"

# Every resource belongs to this invocation; no persistent host Cargo/mbx caches.
# With ZELVIK_EDGE_TEST_KEEP_CACHE=1 the tools/rustup volumes are reused by name
# (no RUN_ID) so nightly and bpf-linker survive; the others stay per-run.
for cache in workspace registry rustup tools mbx; do
    volume="zelvik-edge-test-$cache-$ARCH"
    keep=0
    case ",$KEEP_CACHE:$cache" in ,1:tools | ,1:rustup) keep=1 ;; esac
    if [ "$keep" -eq 0 ]; then
        volume="$volume-$RUN_ID"
        docker --context orbstack volume create "$volume" >/dev/null
    elif ! docker --context orbstack volume inspect "$volume" >/dev/null 2>&1; then
        docker --context orbstack volume create "$volume" >/dev/null
    fi
    VOLUMES+=("$volume")
done
docker --context orbstack run --rm --platform "linux/$ARCH" \
    --name "zelvik-edge-orb-test-$ARCH-$RUN_ID" --cidfile "$CIDFILE" \
    --cpus "${ZELVIK_EDGE_BUILD_CPUS:-2}" --memory "${ZELVIK_EDGE_BUILD_MEMORY:-4g}" \
    --mount "type=bind,source=$ROOT,target=/source,readonly" \
    ${MIRROR_ARGS[@]+"${MIRROR_ARGS[@]}"} \
    --mount "type=bind,source=$RESULTS,target=/results" \
    --mount "type=volume,source=${VOLUMES[0]},target=/workspace" \
    --mount "type=volume,source=${VOLUMES[1]},target=/usr/local/cargo/registry" \
    --mount "type=volume,source=${VOLUMES[2]},target=/usr/local/rustup" \
    --mount "type=volume,source=${VOLUMES[3]},target=/tooling" \
    --mount "type=volume,source=${VOLUMES[4]},target=/var/cache/zelvik-mbx" \
    -e "ZELVIK_EDGE_TEST_PHASE=$PHASE" -e "ZELVIK_EDGE_TEST_ARCH=$ARCH" \
    -e "SOURCE_DATE_EPOCH=$(git -C "$ROOT" log -1 --format=%ct)" \
    "$IMAGE" bash /source/scripts/test-orbstack-container.sh \
    2>&1 | tee "$RESULTS/container.log"
printf 'Local test results: %s\n' "$RESULTS"
