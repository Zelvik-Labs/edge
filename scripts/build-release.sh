#!/usr/bin/env bash
# zelvik-edge release package builder.
#
# Produces zelvik-edge-<arch>-unknown-linux-gnu.tar.gz with the layout
# expected by the control-plane bootstrap and deploy/install.sh:
#
#   zelvik-edge
#   install.sh                      <- deploy/install.sh
#   systemd/zelvik-edge.service
#   data/zelvik-edge-xdp-ebpf.o     (included when present)
#   README.install
#
# Also writes a companion .sha256 file and prints the sha256 + size.
#
# On macOS the script delegates the actual Rust build to
# build-release-local.sh (cargo zigbuild cross-compile). On Linux it runs
# a native cargo build.  Both paths honour the same --arch / --version flags.
#
# Usage:
#   bash scripts/build-release.sh
#   bash scripts/build-release.sh --arch aarch64 --version 26.10.2
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="26.10.2"
ARCH=""
OUT_DIR="${RELEASE_OUT_DIR:-${REPO_ROOT}/release}"

usage() {
    cat >&2 <<'EOF'
Usage: build-release.sh [--arch x86_64|aarch64] [--version VERSION]

  --arch     Target architecture: x86_64 or aarch64 (default: host arch)
  --version  Version string embedded in the package name (default: 26.10.2)
EOF
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --arch=*)    ARCH="${1#*=}"; shift ;;
        --arch)      [ "$#" -ge 2 ] || usage; ARCH="$2"; shift 2 ;;
        --version=*) VERSION="${1#*=}"; shift ;;
        --version)   [ "$#" -ge 2 ] || usage; VERSION="$2"; shift 2 ;;
        -h|--help)   usage ;;
        *) printf 'unknown option: %s\n' "$1" >&2; usage ;;
    esac
done

# Resolve default arch from the host.
HOST_OS="$(uname -s)"
if [ -z "$ARCH" ]; then
    HOST_ARCH="$(uname -m)"
    case "$HOST_ARCH" in
        x86_64|amd64) ARCH="x86_64" ;;
        aarch64|arm64) ARCH="aarch64" ;;
        *) printf 'unsupported host arch: %s — pass --arch explicitly\n' "$HOST_ARCH" >&2; exit 1 ;;
    esac
fi

case "$ARCH" in
    x86_64|aarch64) ;;
    *) printf 'unsupported --arch: %s (want x86_64 or aarch64)\n' "$ARCH" >&2; exit 1 ;;
esac

TRIPLE="${ARCH}-unknown-linux-gnu"
PKG_STEM="zelvik-edge-${ARCH}-unknown-linux-gnu"
PKG_NAME="${PKG_STEM}.tar.gz"

mkdir -p "$OUT_DIR"

# ---------------------------------------------------------------------------
# Step 1: build the binary
# ---------------------------------------------------------------------------
printf '[build-release] building %s for %s ...\n' "$PKG_STEM" "$TRIPLE"

if [ "$HOST_OS" = "Darwin" ]; then
    # macOS: reuse the existing cross-compile script which handles sysroot,
    # zigbuild, eBPF object compilation, and cargo flags.
    # build-release-local.sh builds x86_64 by default; for aarch64 we need
    # the aarch64 sysroot path — delegate but override the target.
    LOCAL_BUILD="${REPO_ROOT}/build-release-local.sh"
    [ -x "$LOCAL_BUILD" ] || { printf 'build-release-local.sh not found or not executable\n' >&2; exit 1; }
    if [ "$ARCH" = "aarch64" ]; then
        printf '[build-release] WARNING: aarch64 cross-compile from macOS requires\n' >&2
        printf '  a separate aarch64 sysroot; see build-release-local.sh for the\n' >&2
        printf '  x86_64 sysroot setup. Set ZELVIK_EDGE_ARCH=aarch64 and ensure\n' >&2
        printf '  ~/tools/sysroot-aarch64-linux-gnu exists before proceeding.\n' >&2
        # Pass through; the user must have set up the aarch64 sysroot.
        GLIBC="${ZELVIK_EDGE_GLIBC:-2.36}"
        export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=generic -L native=${HOME}/tools/sysroot-aarch64-linux-gnu/usr/lib/aarch64-linux-gnu"
        export CFLAGS_aarch64_unknown_linux_gnu="-I${HOME}/tools/sysroot-aarch64-linux-gnu/usr/include"
        cd "$REPO_ROOT"
        cargo zigbuild --release --target "aarch64-unknown-linux-gnu.${GLIBC}" --bin zelvik-edge
        BIN_PATH="${REPO_ROOT}/target/aarch64-unknown-linux-gnu/release/zelvik-edge"
    else
        cd "$REPO_ROOT"
        bash "$LOCAL_BUILD"
        GLIBC="${ZELVIK_EDGE_GLIBC:-2.36}"
        BIN_PATH="${REPO_ROOT}/target/x86_64-unknown-linux-gnu.${GLIBC}/release/zelvik-edge"
        # build-release-local.sh may put the binary under the versioned target triple
        # or the plain triple; find whichever exists.
        if [ ! -f "$BIN_PATH" ]; then
            BIN_PATH="${REPO_ROOT}/target/x86_64-unknown-linux-gnu/release/zelvik-edge"
        fi
    fi
else
    # Linux: native cargo build.
    cd "$REPO_ROOT"
    cargo build --release --bin zelvik-edge
    BIN_PATH="${REPO_ROOT}/target/release/zelvik-edge"
fi

[ -f "$BIN_PATH" ] || { printf 'binary not found after build: %s\n' "$BIN_PATH" >&2; exit 1; }
printf '[build-release] binary: %s (%d bytes)\n' "$BIN_PATH" "$(wc -c < "$BIN_PATH")"

# ---------------------------------------------------------------------------
# Step 2: resolve eBPF object (embedded in binary at build time on Linux;
# the separate .o is an optional override / debugging artifact)
# ---------------------------------------------------------------------------
EBPF_OBJ="${REPO_ROOT}/data/zelvik-edge-xdp-ebpf.o"

# ---------------------------------------------------------------------------
# Step 3: assemble staging directory
# ---------------------------------------------------------------------------
STAGE_DIR="$(mktemp -d)"
trap 'rm -rf "$STAGE_DIR"' EXIT

cp "$BIN_PATH" "${STAGE_DIR}/zelvik-edge"
chmod 0755 "${STAGE_DIR}/zelvik-edge"

cp "${REPO_ROOT}/deploy/install.sh" "${STAGE_DIR}/install.sh"
chmod 0755 "${STAGE_DIR}/install.sh"

mkdir -p "${STAGE_DIR}/systemd"
cp "${REPO_ROOT}/deploy/systemd/zelvik-edge.service" "${STAGE_DIR}/systemd/zelvik-edge.service"

if [ -f "$EBPF_OBJ" ]; then
    mkdir -p "${STAGE_DIR}/data"
    cp "$EBPF_OBJ" "${STAGE_DIR}/data/zelvik-edge-xdp-ebpf.o"
fi

cat > "${STAGE_DIR}/README.install" <<EOF
Zelvik-Edge ${VERSION} — release package

Installed by the control-plane bootstrap via:
  curl -fsSL <cp>/install/edge.sh | sudo bash -s -- \\
    --endpoint <cp-url> --node-id <id> --secret <hex>

Or directly after extracting this archive:
  sudo ./install.sh --endpoint <cp-url> --node-id <id> --secret <hex>

Layout installed by install.sh:
  /usr/bin/zelvik-edge                   binary
  /etc/zelvik/edge/secrets.env           node credentials (0600)
  /etc/zelvik/edge/runtime.yaml          runtime config
  /etc/systemd/system/zelvik-edge.service
  /var/lib/zelvik/edge/                  data / cache

Service management:
  systemctl status zelvik-edge
  journalctl -u zelvik-edge -f

Uninstall:
  systemctl disable --now zelvik-edge
  rm -f /usr/bin/zelvik-edge \\
        /etc/systemd/system/zelvik-edge.service \\
        /etc/zelvik/edge/secrets.env \\
        /etc/zelvik/edge/runtime.yaml
  # Remove data directory only if you want to discard cache and state:
  # rm -rf /var/lib/zelvik/edge /var/log/zelvik
  # userdel zelvik-edge
  systemctl daemon-reload
EOF

# ---------------------------------------------------------------------------
# Step 4: create tar
# ---------------------------------------------------------------------------
OUT_TAR="${OUT_DIR}/${PKG_NAME}"
OUT_SHA="${OUT_DIR}/${PKG_NAME%.tar.gz}.sha256"

# Use -C so the archive root contains the files directly (no top-level dir).
tar -czf "$OUT_TAR" -C "$STAGE_DIR" .

SHA256="$(shasum -a 256 "$OUT_TAR" | awk '{print $1}')"
SIZE="$(wc -c < "$OUT_TAR")"
printf '%s  %s\n' "$SHA256" "$PKG_NAME" > "$OUT_SHA"

printf '[build-release] archive: %s\n' "$OUT_TAR"
printf '[build-release] sha256:  %s\n' "$SHA256"
printf '[build-release] size:    %d bytes\n' "$SIZE"
printf '[build-release] layout:\n'
tar -tzf "$OUT_TAR"
