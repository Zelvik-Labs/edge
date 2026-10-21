#!/usr/bin/env bash
# Single source of truth for the product release version.
#
# Cargo's package.version is three-segment ("26.10.3") and cannot express the
# optional CalVer patch segment. VERSION-MANIFEST.toml owns the complete
# product_version ("26.10.3.1"); Cargo.toml owns its three-segment base. This
# script is the only place that joins them, so the release build, CI, the
# installer and the consistency checks all read one value.
#
# Usage:
#   . scripts/release-version.sh          # exports ZELVIK_RELEASE_VERSION
#   scripts/release-version.sh --print    # 26.10.3.1
#   scripts/release-version.sh --cargo-version
set -euo pipefail

_rv_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
_rv_manifest="${_rv_root}/VERSION-MANIFEST.toml"
_rv_cargo="${_rv_root}/Cargo.toml"

_rv_fail() {
    printf 'release-version: %s\n' "$*" >&2
    return 1
}

# [package] version only: the first "version =" after the [package] header.
ZELVIK_CARGO_VERSION="$(awk '
    /^\[package\]/ { in_pkg = 1; next }
    /^\[/ { in_pkg = 0 }
    in_pkg && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' "$_rv_cargo")"
[ -n "$ZELVIK_CARGO_VERSION" ] || { _rv_fail "Cargo.toml has no [package] version"; exit 1; }

_rv_field() {
    sed -n "s/^$1[[:space:]]*=[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$_rv_manifest" | head -1
}
ZELVIK_PRODUCT_VERSION="$(_rv_field product_version)"
_rv_cargo_declared="$(_rv_field cargo_version)"
_rv_tag="$(_rv_field release_tag)"

[ -n "$ZELVIK_PRODUCT_VERSION" ] || { _rv_fail "VERSION-MANIFEST.toml has no product_version"; exit 1; }
[ "$_rv_cargo_declared" = "$ZELVIK_CARGO_VERSION" ] || {
    _rv_fail "manifest cargo_version ${_rv_cargo_declared:-<missing>} != Cargo.toml ${ZELVIK_CARGO_VERSION}"
    exit 1
}
[[ "$ZELVIK_PRODUCT_VERSION" =~ ^[0-9]{2}\.(1[0-2]|[1-9])\.([1-5])(\.[1-9][0-9]*)?$ ]] || {
    _rv_fail "product_version ${ZELVIK_PRODUCT_VERSION} is not CalVer YY.M.W[.P]"
    exit 1
}
[ "$_rv_tag" = "v${ZELVIK_PRODUCT_VERSION}" ] || {
    _rv_fail "release_tag ${_rv_tag:-<missing>} must be v${ZELVIK_PRODUCT_VERSION}"
    exit 1
}
case "$ZELVIK_PRODUCT_VERSION" in
    "$ZELVIK_CARGO_VERSION"|"$ZELVIK_CARGO_VERSION".*) ;;
    *) _rv_fail "product_version ${ZELVIK_PRODUCT_VERSION} does not extend Cargo version ${ZELVIK_CARGO_VERSION}"; exit 1 ;;
esac

# An override that disagrees with the manifest is drift, not a build option.
if [ -n "${ZELVIK_RELEASE_VERSION:-}" ] && [ "${ZELVIK_RELEASE_VERSION}" != "$ZELVIK_PRODUCT_VERSION" ]; then
    _rv_fail "ZELVIK_RELEASE_VERSION=${ZELVIK_RELEASE_VERSION} disagrees with manifest ${ZELVIK_PRODUCT_VERSION}"
    exit 1
fi
export ZELVIK_RELEASE_VERSION="$ZELVIK_PRODUCT_VERSION"
export ZELVIK_CARGO_VERSION
export ZELVIK_RELEASE_TAG="$_rv_tag"

case "${1:-}" in
    --print) printf '%s\n' "$ZELVIK_RELEASE_VERSION" ;;
    --cargo-version) printf '%s\n' "$ZELVIK_CARGO_VERSION" ;;
    --tag) printf '%s\n' "$ZELVIK_RELEASE_TAG" ;;
    "") ;;
    *) printf 'usage: %s [--print|--cargo-version|--tag]\n' "$0" >&2; exit 1 ;;
esac
