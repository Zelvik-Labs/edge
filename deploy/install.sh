#!/usr/bin/env bash
# Zelvik-Edge single-node systemd installer.
# Installs the Zelvik-Edge binary, dedicated user + dirs, a standalone
# runtime.yaml, and the systemd unit. Reads node_id / secret / API endpoints
# from env or an interactive prompt and renders /etc/zelvik/edge/secrets.env.
# Keep the existing upstream installer (scripts/install-Zelvik-Edge.sh) for
# migration of legacy Go cloud-node; this one is the Zelvik systemd path.
set -euo pipefail

BIN_NAME="zelvik-edge"
SERVICE_NAME="${SERVICE_NAME:-zelvik-edge}"
SERVICE_USER="${SERVICE_USER:-zelvik-edge}"
SERVICE_GROUP="${SERVICE_GROUP:-zelvik-edge}"
INSTALL_DIR="${INSTALL_DIR:-/usr/bin}"
CONFIG_DIR="${CONFIG_DIR:-/etc/zelvik/edge}"
DATA_DIR="${DATA_DIR:-/var/lib/zelvik/edge}"
LOG_DIR="${LOG_DIR:-/var/log/zelvik}"
UNIT_SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/systemd/${SERVICE_NAME}.service"
RUNTIME_FILE="${CONFIG_DIR}/runtime.yaml"
ENV_FILE="${CONFIG_DIR}/secrets.env"
API_ENDPOINTS="${API_ENDPOINTS:-http://127.0.0.1:8080}"
NODE_ID="${NODE_ID:-}"
NODE_SECRET="${NODE_SECRET:-}"
GEOIP_DIR="${GEOIP_DIR:-${DATA_DIR}/geoip}"
DRY_RUN="${DRY_RUN:-0}"

log() { printf '[install] %s\n' "$*"; }
die() { printf '[install][error] %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"

if [ -z "$NODE_ID" ] || [ -z "$NODE_SECRET" ]; then
  if [ -t 0 ]; then
    NODE_ID="${NODE_ID:-$(read -rp 'nodeId: ')}" || true
    read -rsp 'node secret: ' NODE_SECRET && printf '\n' || true
  else
    die "NODE_ID and NODE_SECRET are required in non-interactive mode"
  fi
fi
[ -n "$NODE_ID" ] || die "nodeId is required"
[ -n "$NODE_SECRET" ] || die "node secret is required"

if [ "$DRY_RUN" -eq 0 ]; then
  id "$SERVICE_USER" >/dev/null 2>&1 || useradd --system --no-create-home --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
  install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
  install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR"
  install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$GEOIP_DIR"
fi

install_binary() {
  local src="${1:?binary source required}"
  [ -x "$src" ] || die "binary not executable: $src"
  if [ "$DRY_RUN" -eq 0 ]; then
    install -m 0755 -o root -g root "$src" "$INSTALL_DIR/$BIN_NAME"
    sha256sum "$INSTALL_DIR/$BIN_NAME"
  fi
}

render_runtime() {
  if [ "$DRY_RUN" -eq 0 ]; then
    cat > "$RUNTIME_FILE" <<EOF
runtime:
  mode: standalone
cluster:
  enabled: false
  type: standalone
  name: edge-standalone-01
  namespace: default
  serviceName: ""
  podNameEnv: ""
  podIpEnv: ""
  internalApi:
    bind: 0.0.0.0:19090
    tokenEnv: ""
  leaderElection:
    leaseName: ""
    leaseDurationSeconds: 15
    renewDeadlineSeconds: 10
    retryPeriodSeconds: 2
  cache:
    localMetaDir: ${DATA_DIR}/cache-meta
    maxFastL1Bytes: 0
    sharedMaxBytes: 100GiB
    minFreeBytes: 10GiB
    ignoreControlPlaneStorageOptions: true
    shardStrategy: hash_mod
    shards:
      - id: shard-00
        path: ${DATA_DIR}/cache-shard-00
        weight: 1
        replicas: 1
xdp:
  enabled: false
EOF
    chmod 0640 "$RUNTIME_FILE"
    chown root:"$SERVICE_GROUP" "$RUNTIME_FILE"
  fi
}

render_secrets() {
  if [ "$DRY_RUN" -eq 0 ]; then
    cat > "$ENV_FILE" <<EOF
# Rendered by deploy/install.sh - root only, never commit.
API_ENDPOINTS=${API_ENDPOINTS}
NODE_ID=${NODE_ID}
NODE_SECRET=${NODE_SECRET}
GEOIP_DIR=${GEOIP_DIR}
EOF
    chmod 0600 "$ENV_FILE"
    chown root:"$SERVICE_GROUP" "$ENV_FILE"
  fi
}

install_unit() {
  [ -f "$UNIT_SRC" ] || die "unit not found: $UNIT_SRC"
  if [ "$DRY_RUN" -eq 0 ]; then
    install -m 0644 -o root -g root "$UNIT_SRC" "/etc/systemd/system/${SERVICE_NAME}.service"
    systemctl daemon-reload
    systemctl enable "$SERVICE_NAME"
  fi
}

SRC_BINARY="${1:-}"
[ -n "$SRC_BINARY" ] || SRC_BINARY="${EDGE_BINARY:-./target/release/Zelvik-Edge}"
[ -f "$SRC_BINARY" ] || die "binary not found: $SRC_BINARY (cargo build --release)"

install_binary "$SRC_BINARY"
render_runtime
render_secrets
install_unit

if [ "$DRY_RUN" -eq 0 ]; then
  systemctl restart "$SERVICE_NAME"
  log "restarted $SERVICE_NAME; check: journalctl -u $SERVICE_NAME -f"
fi
log "installed $SERVICE_NAME"
