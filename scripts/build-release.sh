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

# ---------- tar flags --------------------------------------------------------
# macOS bsdtar embeds extended attributes (e.g. com.apple.provenance) into the
# archive as LIBARCHIVE.xattr.* records. GNU tar on Linux then prints
# "Ignoring unknown extended header keyword 'LIBARCHIVE.xattr.com.apple.provenance'"
# once per file, polluting the bootstrap/installer output. COPYFILE_DISABLE=1
# stops bsdtar writing ._ AppleDouble entries; --no-xattrs stops it embedding the
# xattr records themselves. Linux GNU tar produces neither, so it needs no flags.
# --no-mac-metadata does NOT help here: it only suppresses ._ sidecar files, not
# the xattr records inside the archive.
TAR_EXTRA=""
if [ "$HOST_OS" = "Darwin" ]; then
    COPYFILE_DISABLE=1
    export COPYFILE_DISABLE
    TAR_EXTRA="--no-xattrs"
fi

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
#
# Cargo's [[bin]] name is case-sensitive: the product binary is `Zelvik-Edge`
# (see Cargo.toml and .github/workflows/release.yml). It is copied into the
# package as `zelvik-edge` below.
# ---------------------------------------------------------------------------
BIN_TARGET="Zelvik-Edge"
printf '[build-release] building %s for %s ...\n' "$PKG_STEM" "$TRIPLE"

# Locate the built binary across the target layouts cargo/zigbuild may use.
locate_bin() {
    for candidate in \
        "${REPO_ROOT}/target/${TRIPLE}/release/${BIN_TARGET}" \
        "${REPO_ROOT}/target/${ARCH}-unknown-linux-gnu/release/${BIN_TARGET}" \
        "${REPO_ROOT}/target/release/${BIN_TARGET}"
    do
        if [ -f "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

if [ "$HOST_OS" = "Darwin" ]; then
    # macOS: reuse the existing cross-compile script, which owns the sysroot,
    # zigbuild, LLVM tool and glibc-floor setup. It cross-compiles x86_64 only
    # (the sysroot under ~/tools/sysroot-x86_64-linux-gnu is x86_64).
    LOCAL_BUILD="${REPO_ROOT}/build-release-local.sh"
    [ -x "$LOCAL_BUILD" ] || { printf 'build-release-local.sh not found or not executable\n' >&2; exit 1; }
    if [ "$ARCH" != "x86_64" ]; then
        printf 'error: macOS cross-compile is wired for x86_64 only.\n' >&2
        printf '       build-release-local.sh uses the x86_64 sysroot\n' >&2
        printf '       (~/tools/sysroot-x86_64-linux-gnu). Build aarch64 in a\n' >&2
        printf '       Linux/OrbStack container instead:\n' >&2
        printf '         bash scripts/test-orbstack.sh arm64 check\n' >&2
        exit 1
    fi
    cd "$REPO_ROOT"
    bash "$LOCAL_BUILD"
else
    # Linux: native build for the requested arch. --target is used so a
    # cross toolchain (if present) picks the right triple; on a native build
    # it resolves to the same directory cargo would use anyway.
    cd "$REPO_ROOT"
    cargo build --release --target "$TRIPLE" --bin "$BIN_TARGET"
fi

BIN_PATH="$(locate_bin)" || {
    printf 'error: binary not found after build (looked for %s under target/)\n' "$BIN_TARGET" >&2
    exit 1
}
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
# shellcheck disable=SC2086  # TAR_EXTRA is intentionally word-split (empty or one flag).
tar $TAR_EXTRA -czf "$OUT_TAR" -C "$STAGE_DIR" .

SHA256="$(shasum -a 256 "$OUT_TAR" | awk '{print $1}')"
SIZE="$(wc -c < "$OUT_TAR")"
printf '%s  %s\n' "$SHA256" "$PKG_NAME" > "$OUT_SHA"

printf '[build-release] archive: %s\n' "$OUT_TAR"
printf '[build-release] sha256:  %s\n' "$SHA256"
printf '[build-release] size:    %d bytes\n' "$SIZE"
printf '[build-release] layout:\n'
tar -tzf "$OUT_TAR"
