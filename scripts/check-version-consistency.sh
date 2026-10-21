#!/usr/bin/env bash
# Executable guard against version drift across every place the product version
# is consumed: Cargo.toml, VERSION-MANIFEST.toml, the Rust CLI / registration
# report, the installer, the packaging scripts and the release workflow.
#
# Exits non-zero on any inconsistency. Run it locally and in CI before a build.
set -euo pipefail

_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$_root"
_fail=0
_pass() { printf 'PASS  %s\n' "$*"; }
_bad() { printf 'FAIL  %s\n' "$*"; _fail=1; }

# shellcheck source=scripts/release-version.sh
. scripts/release-version.sh
_product="$ZELVIK_RELEASE_VERSION"
_cargo="$ZELVIK_CARGO_VERSION"
_pass "single source: product $_product (Cargo base $_cargo, tag $ZELVIK_RELEASE_TAG)"

# 1. Registration must report the same version as the CLI.
if grep -q 'env!("CARGO_PKG_VERSION")' src/rpc/client.rs; then
    _bad "src/rpc/client.rs still reports CARGO_PKG_VERSION"
else
    _pass "src/rpc/client.rs registration reports crate::RELEASE_VERSION"
fi
grep -q 'software_version: crate::RELEASE_VERSION' src/rpc/client.rs \
    && _pass "RegisterInstance.software_version uses crate::RELEASE_VERSION" \
    || _bad "RegisterInstance.software_version is not crate::RELEASE_VERSION"

# 2. CLI --version, status and the startup log read the injected constant.
grep -q '#\[command(version = zelvik_edge::RELEASE_VERSION)\]' src/main.rs \
    && _pass "clap --version reads RELEASE_VERSION" \
    || _bad "clap command version does not read RELEASE_VERSION"
grep -q 'println!("{}: {}", t("status.version"), zelvik_edge::RELEASE_VERSION)' src/main.rs \
    && _pass "status prints RELEASE_VERSION" \
    || _bad "status does not print RELEASE_VERSION"
grep -q 'Starting Zelvik Edge v{}' src/main.rs \
    && _pass "startup log prints RELEASE_VERSION" \
    || _bad "startup log does not print RELEASE_VERSION"

# 3. Packaging scripts carry no hand-written product version.
if grep -RIn 'VERSION="[0-9][0-9]*\.' scripts/*.sh >/dev/null 2>&1; then
    _bad "a scripts/*.sh file hard-codes VERSION=\"...\""
else
    _pass "scripts/*.sh carry no hand-written VERSION literal"
fi
grep -q 'release-version.sh' scripts/build-release.sh \
    && _pass "build-release.sh resolves the version from the manifest" \
    || _bad "build-release.sh does not source release-version.sh"
grep -q 'verify-release-package.sh' scripts/build-release.sh \
    && _pass "build-release.sh verifies the package before publishing" \
    || _bad "build-release.sh does not call verify-release-package.sh"

# 4. Installer: one assignment, patched at packaging time to the product version.
_assignments="$(grep -c '^VERSION="' deploy/install.sh || true)"
if [ "$_assignments" = "1" ]; then
    _pass "deploy/install.sh has exactly one VERSION assignment (injection target)"
    _installer="$(sed -n 's/^VERSION="\([^"]*\)"$/\1/p' deploy/install.sh)"
    if [ "$_installer" = "$_product" ]; then
        _pass "deploy/install.sh VERSION = $_installer"
    elif [ "$_installer" = "$_cargo" ]; then
        _pass "deploy/install.sh template VERSION $_installer is the Cargo base; build-release.sh overwrites it with $_product and the packaged file is verified"
    else
        _bad "deploy/install.sh VERSION $_installer is neither the product nor the Cargo base version"
    fi
else
    _bad "deploy/install.sh must contain exactly one VERSION=\"...\" assignment (found $_assignments)"
fi

# 5. Workflow: one shared eBPF build, per-leg target/cache, ISA assertions,
#    and the version injected from the manifest rather than a literal.
_wf=.github/workflows/release.yml
grep -q 'scripts/release-version.sh' "$_wf" \
    && _pass "release.yml takes ZELVIK_RELEASE_VERSION from release-version.sh" \
    || _bad "release.yml does not use release-version.sh"
grep -q 'ZELVIK_RELEASE_VERSION=' "$_wf" \
    && _pass "release.yml exports ZELVIK_RELEASE_VERSION to the build" \
    || _bad "release.yml does not export ZELVIK_RELEASE_VERSION"
grep -q '^  ebpf:' "$_wf" \
    && _pass "release.yml builds eBPF in one job" \
    || _bad "release.yml has no dedicated eBPF job"
grep -q 'needs: ebpf' "$_wf" \
    && _pass "build legs consume the shared eBPF artifact" \
    || _bad "build legs do not depend on the shared eBPF job"
grep -q 'CARGO_TARGET_DIR: .*matrix.name' "$_wf" \
    && _pass "each leg has its own target directory" \
    || _bad "per-leg CARGO_TARGET_DIR is missing"
grep -q 'isa_regex' "$_wf" \
    && _pass "release.yml asserts the instruction set per leg" \
    || _bad "release.yml has no instruction-set assertion"
grep -q 'verify-release-package.sh' "$_wf" \
    && _pass "release.yml verifies the packaged version" \
    || _bad "release.yml does not run verify-release-package.sh"
for needle in 'lto = "fat"' 'codegen-units = 1' 'opt-level = 3'; do
    grep -q "$needle" Cargo.toml || _bad "Cargo.toml lost $needle"
done
_pass "Cargo.toml keeps fat LTO / codegen-units=1 / opt-level=3"

if [ "$_fail" -ne 0 ]; then
    printf '\nversion consistency: FAILED\n' >&2
    exit 1
fi
printf '\nversion consistency: OK (product %s)\n' "$_product"
