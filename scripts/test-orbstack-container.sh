#!/usr/bin/env bash
# Invoked in an isolated OrbStack build container by test-orbstack.sh.
set -euo pipefail
export PATH="/tooling/bin:$PATH"
PHASE="${ZELVIK_EDGE_TEST_PHASE:-all}"
ARCH="${ZELVIK_EDGE_TEST_ARCH:?architecture is required}"
OVERALL_STATUS=0
LAST_CLEANUP_STATUS=0

# The named workspace volume is disposable build storage, not the source bind.
rsync -a --delete --exclude=target --exclude=.git --exclude=.DS_Store \
    --exclude=/data --exclude=/runtime --exclude=/configs --exclude=/api_node.yaml \
    --exclude=.env --exclude='.env.*' /source/ /workspace/
cd /workspace
export CLANG=/usr/bin/clang LD=/usr/bin/ld.lld OBJCOPY=/usr/bin/llvm-objcopy
case "$ARCH" in
    arm64) export RUSTFLAGS='-C target-cpu=generic -C link-arg=-fuse-ld=lld' ;;
    amd64) export RUSTFLAGS='-C target-cpu=x86-64-v2 -C link-arg=-fuse-ld=lld' ;;
    *) printf 'unsupported architecture: %s\n' "$ARCH" >&2; exit 2 ;;
esac

cleanup() {
    local status=0 workspace
    # CARGO_TARGET_DIR must not leak from one Cargo workspace into another.
    for workspace in /workspace /workspace/xtask /workspace/pingora-main \
        /workspace/crates/zelvik-edge-xdp-ebpf; do
        env -u CARGO_TARGET_DIR bash /workspace/scripts/clean-build-artifacts.sh \
            --workspace "$workspace" || status=$?
    done
    mbx gc --json > /results/gc.json || status=$?
    return "$status"
}
finish() {
    local status=$? cleanup_status=0
    trap - EXIT
    cleanup > /results/final-cleanup.log 2>&1 || cleanup_status=$?
    mbx stats > /results/mbx-stats.txt || cleanup_status=$?
    printf '%s\n' "$status" > /results/test-exit-status.txt
    printf '%s\n' "$cleanup_status" > /results/cleanup-exit-status.txt
    if [ "$status" -ne 0 ]; then exit "$status"; fi
    exit "$cleanup_status"
}
trap finish EXIT

run_build() {
    local label="$1" status=0 cleanup_status=0
    shift
    "$@" > "/results/$label.log" 2>&1 || status=$?
    cleanup > "/results/$label-cleanup.log" 2>&1 || cleanup_status=$?
    LAST_CLEANUP_STATUS="$cleanup_status"
    printf '%s\n' "$status" > "/results/$label-exit-status.txt"
    printf '%s: build=%s cleanup=%s\n' "$label" "$status" "$cleanup_status"
    if [ "$status" -ne 0 ]; then return "$status"; fi
    return "$cleanup_status"
}

run_verification() {
    # Independent checks continue after a failure; prerequisites and unsafe
    # cleanup failures still stop immediately. The final exit remains nonzero.
    run_build "$@" || OVERALL_STATUS=$?
    if [ "$LAST_CLEANUP_STATUS" -ne 0 ]; then exit "$LAST_CLEANUP_STATUS"; fi
}

protocol_preflight() {
    source /workspace/scripts/orbstack-protocol.sh || return
    configure_protocol_git /workspace/Cargo.lock /protocol.git || return
    # Resolve every dependency without compiling. A missing private object, registry
    # failure or changed lockfile stops all phases before nightly/xtask/eBPF work.
    cargo metadata --locked --format-version 1 > /results/dependency-metadata.json
}

rustc --version
cargo --version
mbx --version
mbx doctor --json > /results/mbx-doctor.json
python3 -I scripts/test-clean-build-artifacts.py > /results/cleanup-tests.log 2>&1
python3 -I scripts/test-orbstack-orchestration.py > /results/orchestration-tests.log 2>&1
# The independent pingora workspace has no private git dependency to resolve.
if [ "$PHASE" != pingora ]; then
    if ! run_build dependencies protocol_preflight; then
        printf 'Dependency preflight failed; see /results/dependencies.log. No build phases started.\n' >&2
        exit 1
    fi
fi

# Resolve the existing dated nightly; stable 1.99 is never used for BPF.
NIGHTLY="$(python3 -I -c 'import tomllib; print(tomllib.load(open("/workspace/crates/zelvik-edge-xdp-ebpf/rust-toolchain.toml", "rb"))["toolchain"]["channel"])')"
rustup toolchain install "$NIGHTLY" --profile minimal -c rust-src -c rustc-dev \
    > /results/ebpf-toolchain.log 2>&1
if [ ! -x /tooling/bin/bpf-linker ] || \
    ! /tooling/bin/bpf-linker --version | grep -q '0.11.1'; then
    SYSROOT="$(rustup run "$NIGHTLY" rustc --print sysroot)"
    LLVM_LIBRARY="$(find "$SYSROOT/lib" -name 'libLLVM.so.*' -print -quit)"
    test -n "$LLVM_LIBRARY"
    ln -sf "$LLVM_LIBRARY" "$SYSROOT/lib/libLLVM.so"
    LLVM_PREFIX="$SYSROOT" rustup run "$NIGHTLY" cargo install bpf-linker \
        --version 0.11.1 --locked --root /tooling > /results/bpf-linker-install.log 2>&1
fi

# Copy xtask out before clearing its target, then invoke it without Cargo's
# wrapper environment. Its nested nightly build cannot contend with a parent.
run_build xtask bash -c \
    'mbx build --locked --manifest-path xtask/Cargo.toml && install -m 0755 xtask/target/debug/xtask /tooling/zelvik-xtask'
run_build ebpf env -u CARGO_TARGET_DIR -u RUSTFLAGS /tooling/zelvik-xtask build-ebpf
export ZELVIK_EDGE_XDP_EBPF_SOURCE=/workspace/data/zelvik-edge-xdp-ebpf.o
test -s "$ZELVIK_EDGE_XDP_EBPF_SOURCE"
ZELVIK_EDGE_XDP_EBPF_SHA256="$(sha256sum "$ZELVIK_EDGE_XDP_EBPF_SOURCE")"
export ZELVIK_EDGE_XDP_EBPF_SHA256="${ZELVIK_EDGE_XDP_EBPF_SHA256%% *}"
printf '%s\n' "$ZELVIK_EDGE_XDP_EBPF_SHA256" > /results/ebpf-sha256.txt

if [ "$PHASE" = all ] || [ "$PHASE" = check ]; then
    run_verification check mbx check --locked --all-targets
    run_verification cache-recheck mbx check --locked --all-targets
fi
if [ "$PHASE" = all ] || [ "$PHASE" = test ]; then
    run_verification unit-tests mbx test --locked --workspace --lib --bins --no-fail-fast
    run_verification integration-tests mbx test --locked --no-fail-fast --test '*'
fi
if [ "$PHASE" = all ] || [ "$PHASE" = clippy ]; then
    run_verification clippy mbx clippy --locked --all-targets -- \
        -D clippy::correctness -D clippy::suspicious
fi
if [ "$PHASE" = all ] || [ "$PHASE" = pingora ]; then
    FEATURES=pingora-core/rustls,pingora-cache/rustls,pingora-proxy/rustls
    run_verification pingora-check mbx check --manifest-path pingora-main/Cargo.toml \
        -p pingora-core -p pingora-http -p pingora-cache -p pingora-proxy \
        --no-default-features --features "$FEATURES" --all-targets
    run_verification pingora-tests mbx test --manifest-path pingora-main/Cargo.toml \
        -p pingora-core -p pingora-http -p pingora-cache -p pingora-proxy \
        --no-default-features --features "$FEATURES" --lib --no-fail-fast
fi
if [ "$OVERALL_STATUS" -ne 0 ]; then
    printf 'Local %s %s verification failed; see per-stage logs.\n' "$ARCH" "$PHASE" >&2
    exit "$OVERALL_STATUS"
fi
printf 'Local %s %s verification completed. No release or deployment performed.\n' "$ARCH" "$PHASE"
