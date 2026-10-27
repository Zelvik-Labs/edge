#!/bin/sh
# Docker-free regression for removal of the node Geo trust-key requirement.
# Optional argument selects an installer for the negative control.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
INSTALLER=${1:-"$ROOT/deploy/install.sh"}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/edge-no-geo-trust.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$WORK/package/systemd"
cp "$INSTALLER" "$WORK/package/install.sh"
cp "$ROOT/deploy/systemd/zelvik-edge.service" "$WORK/package/systemd/"
printf '0.0.0\n' > "$WORK/package/VERSION"
printf '#!/bin/sh\n[ "$1" = --version ] && printf "zelvik-edge 0.0.0\\n"\n' > "$WORK/package/zelvik-edge"
chmod 0755 "$WORK/package/zelvik-edge"

# A stale environment variable must no longer block preflight or installation.
if ! ZELVIK_EDGE_TRUST_KEYS=bad sh "$WORK/package/install.sh" \
    --dry-run --install-dir "$WORK/home" --endpoint http://127.0.0.1:8744 \
    --comm-mode plaintext --node-id nd_test --secret test-secret > "$WORK/dry-run.log" 2>&1; then
    printf 'FAIL obsolete Geo environment blocked dry-run\n' >&2
    cat "$WORK/dry-run.log" >&2
    exit 1
fi
grep -qF 'zelvik-edge 0.0.0 would be installed' "$WORK/dry-run.log"
[ ! -e "$WORK/home" ]
printf 'OK obsolete Geo environment does not block dry-run\n'

for option in --trust-keys --trust-keys=bad; do
    if sh "$WORK/package/install.sh" "$option" bad > "$WORK/option.log" 2>&1; then
        printf 'FAIL removed option accepted: %s\n' "$option" >&2
        exit 1
    fi
    grep -qF "unknown option: $option" "$WORK/option.log"
done
printf 'OK removed Geo options are rejected\n'

sh "$WORK/package/install.sh" --help > "$WORK/help.log" 2>&1 || :
if grep -Eq -- 'trust-keys|ZELVIK_EDGE_TRUST_KEYS' "$WORK/help.log"; then
    printf 'FAIL installer still advertises Geo trust keys\n' >&2
    exit 1
fi
printf 'OK help no longer advertises Geo trust keys\n'
