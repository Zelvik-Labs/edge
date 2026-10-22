#!/usr/bin/env bash
# Exercise the shipped service's actual systemd permissions using fake binaries.
# No Edge instance is enrolled and no running zelvik-edge service is touched.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [ "$(uname -s)" != Linux ] || [ "$(id -u)" != 0 ] || [ ! -d /run/systemd/system ]; then
    printf '%s\n' 'SKIP: requires Linux with root and systemd as PID 1'
    exit 0
fi

WORK="$(mktemp -d /var/lib/zelvik-root-upgrade-test.XXXXXX)"
NAME="zelvik-root-upgrade-test-${WORK##*.}"
UNIT="$NAME.service"
GUARD="$NAME-guard.service"
BACKUP="/var/backups/zelvik/edge-upgrade/$NAME"
BACKUP_PARENT_CREATED=0
[ -d /var/backups/zelvik ] || BACKUP_PARENT_CREATED=1
UPGRADE_PARENT_CREATED=0
[ -d /var/backups/zelvik/edge-upgrade ] || UPGRADE_PARENT_CREATED=1

cleanup() {
    systemctl stop "$GUARD" "$UNIT" >/dev/null 2>&1 || true
    rm -f "/run/systemd/system/$UNIT"
    systemctl daemon-reload
    systemctl reset-failed "$GUARD" "$UNIT" >/dev/null 2>&1 || true
    rm -rf "$WORK" "$BACKUP"
    if [ "$UPGRADE_PARENT_CREATED" = 1 ]; then rmdir /var/backups/zelvik/edge-upgrade 2>/dev/null || true; fi
    if [ "$BACKUP_PARENT_CREATED" = 1 ]; then rmdir /var/backups/zelvik 2>/dev/null || true; fi
}
trap cleanup EXIT

install -d -m 0755 "$WORK/bin" "$WORK/data" "$WORK/logs"
# Record the old executable mode too: a backup with no execute bit cannot run
# as the detached upgrade guard, even as root.
cat > "$WORK/fixture" <<'FIXTURE'
#!/bin/bash
set -euo pipefail
VERSION=old
if [ "${1:-}" = --version ]; then printf '%s\n' "$VERSION"; exit 0; fi
if [ "${1:-}" = upgrade-guard ]; then
    test "$(id -u)" = 0
    test "$(cat "$ZELVIK_TEST_BACKUP/manifest")" = installed
    systemctl restart "$ZELVIK_TEST_UNIT"
    test "$("$ZELVIK_EDGE_HOME/bin/zelvik-edge" --version)" = new
    for _ in $(seq 1 40); do
        if [ -f "$ZELVIK_EDGE_HOME/data/new-runtime" ]; then break; fi
        sleep 0.25
    done
    test "$(cat "$ZELVIK_EDGE_HOME/data/new-runtime")" = root
    printf '%s\n' completed > "$ZELVIK_EDGE_HOME/data/result"
    exit 0
fi
test "$(id -u)" = 0
if [ "$VERSION" = new ]; then
    printf '%s\n' root > "$ZELVIK_EDGE_HOME/data/new-runtime"
    exec sleep infinity
fi
# Create the same backup location and write the same installation directories
# used by node_upgrade.rs before handing restart to a detached systemd unit.
install -d -m 0700 "$ZELVIK_TEST_BACKUP"
cp -p "$ZELVIK_EDGE_HOME/bin/zelvik-edge" "$ZELVIK_TEST_BACKUP/old-binary"
cp "$ZELVIK_EDGE_HOME/data/zelvik-edge-xdp-ebpf.o" "$ZELVIK_TEST_BACKUP/old-object"
install -m 0755 "$ZELVIK_EDGE_HOME/candidate" "$ZELVIK_EDGE_HOME/bin/zelvik-edge.upgrade-new"
mv "$ZELVIK_EDGE_HOME/bin/zelvik-edge.upgrade-new" "$ZELVIK_EDGE_HOME/bin/zelvik-edge"
printf '%s\n' new-object > "$ZELVIK_EDGE_HOME/data/object.upgrade-new"
mv "$ZELVIK_EDGE_HOME/data/object.upgrade-new" "$ZELVIK_EDGE_HOME/data/zelvik-edge-xdp-ebpf.o"
printf '%s\n' installed > "$ZELVIK_TEST_BACKUP/manifest"
systemd-run --quiet --collect --unit="$ZELVIK_TEST_GUARD" \
    --setenv="ZELVIK_EDGE_HOME=$ZELVIK_EDGE_HOME" \
    --setenv="ZELVIK_TEST_BACKUP=$ZELVIK_TEST_BACKUP" \
    --setenv="ZELVIK_TEST_UNIT=$ZELVIK_TEST_UNIT" \
    "$ZELVIK_TEST_BACKUP/old-binary" upgrade-guard
exec sleep infinity
FIXTURE
install -m 0755 "$WORK/fixture" "$WORK/bin/zelvik-edge"
sed 's/^VERSION=old$/VERSION=new/' "$WORK/fixture" > "$WORK/candidate"
chmod 0755 "$WORK/candidate"
printf '%s\n' old-object > "$WORK/data/zelvik-edge-xdp-ebpf.o"

sed -e "s|@ZELVIK_EDGE_HOME@|$WORK|g" \
    -e "s|@ZELVIK_EDGE_BIN@|$WORK/bin/zelvik-edge|g" \
    "$ROOT/deploy/systemd/zelvik-edge.service" > "/run/systemd/system/$UNIT"
# These test-only environment values select disposable files and units.
cat >> "/run/systemd/system/$UNIT" <<EOF

[Service]
Environment=ZELVIK_TEST_BACKUP=$BACKUP
Environment=ZELVIK_TEST_UNIT=$UNIT
Environment=ZELVIK_TEST_GUARD=$GUARD
Restart=no
EOF
systemd-analyze verify "/run/systemd/system/$UNIT"
systemctl daemon-reload
systemctl start "$UNIT"

for _ in $(seq 1 40); do
    if [ -f "$WORK/data/result" ]; then break; fi
    sleep 0.25
done
if [ ! -f "$WORK/data/result" ]; then
    journalctl -u "$UNIT" -u "$GUARD" -n 50 --no-pager >&2
    printf '%s\n' 'FAIL: root upgrade did not complete' >&2
    exit 1
fi
[ "$(cat "$WORK/data/result")" = completed ]
[ "$(cat "$WORK/data/new-runtime")" = root ]
[ "$(cat "$WORK/data/zelvik-edge-xdp-ebpf.o")" = new-object ]
[ "$(cat "$BACKUP/old-object")" = old-object ]
[ "$("$BACKUP/old-binary" --version)" = old ]
[ "$(systemctl show -p User --value "$UNIT")" = root ]
PID="$(systemctl show -p MainPID --value "$UNIT")"
[ "$PID" -gt 0 ]
[ "$(ps -o uid= -p "$PID" | tr -d ' ')" = 0 ]
printf '%s\n' 'PASS: root backup, binary/object replacement, detached systemd restart and new root process'
