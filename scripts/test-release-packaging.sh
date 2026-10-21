#!/usr/bin/env bash
# Behaviour tests for resolver sourcing and release-package metadata.
# These fixtures are not Rust binaries and do not constitute binary acceptance.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TEST_DIR="$(mktemp -d "$ROOT/.release-package-test.XXXXXX")"
trap 'rm -rf "$TEST_DIR"' EXIT
VERSION="$(bash "$ROOT/scripts/release-version.sh" --print)"
VERIFY="$ROOT/scripts/verify-release-package.sh"
PKG="$TEST_DIR/Zelvik-Edge-linux-x64-v2-sse4.2.tar.gz"
STAGE="$TEST_DIR/stage"
mkdir -p "$STAGE/systemd"

# Source the real resolver with the caller's real arguments. It must neither
# exit nor consume/change them (build-release.sh passes these arguments).
bash -c '
    set -euo pipefail
    resolver="$1"
    expected="$2"
    set -- --arch x86_64 --version "$expected"
    . "$resolver"
    test "$ZELVIK_RELEASE_VERSION" = "$expected"
    test "$#" = 4
    test "$1" = --arch
    test "$2" = x86_64
    test "$3" = --version
    test "$4" = "$expected"
    printf "PASS source with caller arguments: version=%s argc=%s argv=%s\n" "$ZELVIK_RELEASE_VERSION" "$#" "$*"
' resolver-test "$ROOT/scripts/release-version.sh" "$VERSION"

# Include both the installer fallback literal and a runtime VERSION-file read,
# matching the layout installer contract. Neither an extra runtime assignment
# nor a caller argument may confuse the metadata verifier.
printf '%s\n' 'fixture only: not a Rust binary' > "$STAGE/zelvik-edge"
printf 'VERSION="%s"\nVERSION="$(cat "$(dirname "$0")/VERSION")"\n' "$VERSION" > "$STAGE/install.sh"
printf '%s\n' "$VERSION" > "$STAGE/VERSION"
printf 'product_version=%s\n' "$VERSION" > "$STAGE/BUILD-METADATA.txt"
cp "$ROOT/deploy/systemd/zelvik-edge.service" "$STAGE/systemd/zelvik-edge.service"

tar -czf "$PKG" -C "$STAGE" .
bash "$VERIFY" "$PKG" --expect-version "$VERSION"
printf 'PASS complete metadata fixture (unit and runtime VERSION read present)\n'

expect_failure() {
    label="$1"
    expected_message="$2"
    if output="$(bash "$VERIFY" "$PKG" --expect-version "$VERSION" 2>&1)"; then
        printf 'FAIL %s: verifier unexpectedly succeeded\n%s\n' "$label" "$output" >&2
        exit 1
    else
        status="$?"
    fi
    test "$status" = 1
    case "$output" in
        *"$expected_message"*) ;;
        *) printf 'FAIL %s: unexpected error\n%s\n' "$label" "$output" >&2; exit 1 ;;
    esac
    printf 'PASS %s: exit=%s %s\n' "$label" "$status" "$expected_message"
}

# Each negative control repacks the same complete fixture with exactly one
# required member omitted or corrupted; it must fail in the executable check.
tar -czf "$PKG" -C "$STAGE" zelvik-edge install.sh VERSION BUILD-METADATA.txt
expect_failure missing-unit 'package is missing systemd/zelvik-edge.service'

tar -czf "$PKG" -C "$STAGE" zelvik-edge install.sh VERSION systemd
expect_failure missing-build-metadata 'package is missing BUILD-METADATA.txt'

tar -czf "$PKG" -C "$STAGE" zelvik-edge install.sh BUILD-METADATA.txt systemd
expect_failure missing-version 'package is missing VERSION'

printf '%s\n' invalid > "$STAGE/VERSION"
tar -czf "$PKG" -C "$STAGE" .
expect_failure mismatched-version "packaged VERSION invalid != product $VERSION"
printf '%s\n' "$VERSION" > "$STAGE/VERSION"

printf 'VERSION="%s"\n' '0.0.0' > "$STAGE/install.sh"
tar -czf "$PKG" -C "$STAGE" .
expect_failure mismatched-installer "install.sh VERSION 0.0.0 != product $VERSION"
printf 'PASS release packaging behaviour tests (fixtures only; binary acceptance pending)\n'
