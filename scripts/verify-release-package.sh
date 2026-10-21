#!/usr/bin/env bash
# Fail-closed verification of a Zelvik-Edge release package.
#
# The shipped binary, the shipped installer, the package's VERSION file and the
# archive name must all carry one product version. Any mismatch exits non-zero,
# so a caller that gates publishing on this script can never ship a package
# whose CLI and installer disagree.
#
# Usage: verify-release-package.sh <package.tar.gz> [--expect-version V] [--require-exec]
set -euo pipefail

_rv_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
_pkg=""
_expect=""
_require_exec=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --expect-version) [ "$#" -ge 2 ] || { echo "verify-package: --expect-version needs a value" >&2; exit 2; }; _expect="$2"; shift 2 ;;
        --require-exec) _require_exec=1; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        -*) echo "verify-package: unknown option $1" >&2; exit 2 ;;
        *) [ -z "$_pkg" ] || { echo "verify-package: unexpected argument $1" >&2; exit 2; }; _pkg="$1"; shift ;;
    esac
done

[ -n "$_pkg" ] || { echo "usage: $0 <package.tar.gz> [--expect-version V] [--require-exec]" >&2; exit 2; }
[ -f "$_pkg" ] || { echo "verify-package: $_pkg does not exist" >&2; exit 1; }

# shellcheck source=scripts/release-version.sh
. "$_rv_root/scripts/release-version.sh"
if [ -n "$_expect" ] && [ "$_expect" != "$ZELVIK_RELEASE_VERSION" ]; then
    echo "verify-package: --expect-version $_expect != manifest $ZELVIK_RELEASE_VERSION" >&2
    exit 1
fi

_fail() { echo "verify-package: FAIL: $*" >&2; exit 1; }
_ok() { echo "verify-package: ok: $*"; }

_base="$(basename "$_pkg")"
case "$_base" in
    zelvik-edge-*.tar.gz) ;;
    *) _fail "package name $_base does not match zelvik-edge-<arch>-unknown-linux-gnu-<version>.tar.gz" ;;
esac
_stem="${_base%.tar.gz}"
_rest="${_stem#zelvik-edge-}"
case "$_rest" in
    *-unknown-linux-gnu-*) ;;
    *) _fail "package name $_base has no <arch>-unknown-linux-gnu-<version> suffix" ;;
esac
_arch="${_rest%%-unknown-linux-gnu-*}"
_name_version="${_rest##*-unknown-linux-gnu-}"
case "$_arch" in
    x86_64|aarch64) ;;
    *) _fail "unsupported package arch $_arch" ;;
esac
[ "$_name_version" = "$ZELVIK_RELEASE_VERSION" ] || _fail "package name version $_name_version != product $ZELVIK_RELEASE_VERSION"
_ok "package name carries product version $ZELVIK_RELEASE_VERSION (arch $_arch)"

_tmp="$(mktemp -d)"
trap 'rm -rf "$_tmp"' EXIT
tar -xzf "$_pkg" -C "$_tmp"
for f in zelvik-edge install.sh VERSION; do
    [ -f "$_tmp/$f" ] || _fail "package is missing $f"
done

_installer_version="$(sed -n 's/^VERSION="\([^"]*\)"$/\1/p' "$_tmp/install.sh" | head -1)"
[ "$_installer_version" = "$ZELVIK_RELEASE_VERSION" ] || _fail "install.sh VERSION ${_installer_version:-<missing>} != product $ZELVIK_RELEASE_VERSION"
_ok "install.sh VERSION = $_installer_version"

_pkg_version="$(tr -d '[:space:]' < "$_tmp/VERSION")"
[ "$_pkg_version" = "$ZELVIK_RELEASE_VERSION" ] || _fail "package VERSION file ${_pkg_version:-<empty>} != product $ZELVIK_RELEASE_VERSION"
_ok "VERSION file = $_pkg_version"

# Run the binary when the build host can execute the target; otherwise require
# the version literal to be embedded and say so loudly.
_host_os="$(uname -s)"
_host_arch="$(uname -m)"
_runnable=0
if [ "$_host_os" = "Linux" ]; then
    case "$_arch:$_host_arch" in
        x86_64:x86_64|x86_64:amd64|aarch64:aarch64|aarch64:arm64) _runnable=1 ;;
    esac
fi
if [ "$_runnable" = 1 ]; then
    _out="$("$_tmp/zelvik-edge" --version)"
    _reported="${_out##* }"
    [ "$_reported" = "$ZELVIK_RELEASE_VERSION" ] || _fail "zelvik-edge --version reported ${_out:-<empty>} (want $ZELVIK_RELEASE_VERSION)"
    _ok "zelvik-edge --version = $_out"
else
    [ "$_require_exec" = 0 ] || _fail "--require-exec but $_host_os/$_host_arch cannot execute a $_arch Linux binary"
    grep -aqF "$ZELVIK_RELEASE_VERSION" "$_tmp/zelvik-edge" || _fail "binary does not embed $ZELVIK_RELEASE_VERSION"
    _ok "binary embeds $ZELVIK_RELEASE_VERSION (execution skipped: cross-compiled on $_host_os/$_host_arch)"
fi

echo "verify-package: PASS $_pkg (version $ZELVIK_RELEASE_VERSION)"
