#!/usr/bin/env bash
# Fail-closed check of a Zelvik-Edge release package's version metadata.
#
# Scope: package layout, packaged VERSION and the installer's VERSION must
# agree with VERSION-MANIFEST.toml. The binary's reported version is proven
# ONLY by executing it on a machine of the targeted CPU class; this script
# never substitutes byte/string matching for that.
#
# Asset names stay the existing official ones (Zelvik-Edge-<variant>.tar.gz for
# release legs, zelvik-edge-<arch>-unknown-linux-gnu.tar.gz for the local
# builder). Neither carries the version; the version lives inside the package.
#
# Usage: verify-release-package.sh <package.tar.gz> [--expect-version V]
set -euo pipefail

_rv_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
_pkg=""
_expect=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --expect-version) [ "$#" -ge 2 ] || { echo "verify-package: --expect-version needs a value" >&2; exit 2; }; _expect="$2"; shift 2 ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        -*) echo "verify-package: unknown option $1" >&2; exit 2 ;;
        *) [ -z "$_pkg" ] || { echo "verify-package: unexpected argument $1" >&2; exit 2; }; _pkg="$1"; shift ;;
    esac
done

[ -n "$_pkg" ] || { echo "usage: $0 <package.tar.gz> [--expect-version V]" >&2; exit 2; }
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
    Zelvik-Edge-*.tar.gz|zelvik-edge-*.tar.gz) _ok "asset name $_base (official naming, no version suffix)" ;;
    *) _fail "asset name $_base is not an official Zelvik-Edge package name" ;;
esac

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
[ "$_pkg_version" = "$ZELVIK_RELEASE_VERSION" ] || _fail "packaged VERSION ${_pkg_version:-<empty>} != product $ZELVIK_RELEASE_VERSION"
_ok "packaged VERSION = $_pkg_version"

if [ -f "$_tmp/BUILD-METADATA.txt" ]; then
    _meta_version="$(sed -n 's/^product_version=//p' "$_tmp/BUILD-METADATA.txt" | head -1)"
    [ "$_meta_version" = "$ZELVIK_RELEASE_VERSION" ] || _fail "BUILD-METADATA product_version ${_meta_version:-<missing>} != product $ZELVIK_RELEASE_VERSION"
    _ok "BUILD-METADATA product_version = $_meta_version"
fi

echo "verify-package: PASS $_pkg (metadata at $ZELVIK_RELEASE_VERSION)"
echo "note: the binary's --version is proven by executing it on a matching CPU, not here"
