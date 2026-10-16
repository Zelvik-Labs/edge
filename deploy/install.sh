#!/bin/sh
# Zelvik-Edge node installer.
# Unpacked from the release tar and executed by the control-plane bootstrap.
# Always run from inside an extracted tar directory; relative paths are safe.
#
# Usage (via control-plane bootstrap):
#   curl -fsSL <cp>/install/edge.sh | sudo bash -s -- \
#       --endpoint <cp-url> --node-id <id> --secret <hex>
#
# Direct invocation after unpacking:
#   sudo ./install.sh --endpoint https://cp.example.com \
#       --node-id abc123 --secret deadbeef
set -eu

VERSION="26.10.2"
BIN_NAME="zelvik-edge"
SERVICE_NAME="zelvik-edge"
SERVICE_USER="zelvik-edge"
SERVICE_GROUP="zelvik-edge"

INSTALL_BIN_DIR="/usr/bin"
CONFIG_DIR="/etc/zelvik/edge"
DATA_DIR="/var/lib/zelvik/edge"
LOG_DIR="/var/log/zelvik"
UNIT_DIR="/etc/systemd/system"

ENDPOINT=""
NODE_ID=""
NODE_SECRET=""
COMM_MODE="tls"
TRUST_KEYS=""
CUSTOM_DIR=""
DRY_RUN=0

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
log()  { printf '[install] %s\n' "$*"; }
die()  { printf '[install][error] %s\n' "$*" >&2; exit 1; }

usage() {
    cat >&2 <<'EOF'
Usage: install.sh [OPTIONS]

  --endpoint <url>                  Control-plane base URL (comma-separated for multiple)
  --node-id <id>                    Node identity  (alias: --instance-id)
  --secret <hex>                    One-time registration secret
  --comm-mode <plaintext|tls|mtls>  Communication mode (default: tls)
  --trust-keys <hex[,hex...]>       Pinned trust keys (optional)
  --dir <path>                      Installation root (overrides layout defaults)
  --dry-run                         Print actions without writing to disk or starting services

Environment variable fallbacks:
  ZELVIK_ENDPOINT or API_ENDPOINTS
  ZELVIK_NODE_ID  or ZELVIK_INSTANCE_ID
  ZELVIK_NODE_SECRET or ZELVIK_INSTANCE_SECRET
EOF
    exit 1
}

# ---------------------------------------------------------------------------
# argument parsing — accept both --flag value and --flag=value
# ---------------------------------------------------------------------------
parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --endpoint=*)   ENDPOINT="${1#*=}"; shift ;;
            --endpoint)     [ "$#" -ge 2 ] || die "--endpoint requires a value"; ENDPOINT="$2"; shift 2 ;;
            --node-id=*)    NODE_ID="${1#*=}"; shift ;;
            --node-id)      [ "$#" -ge 2 ] || die "--node-id requires a value"; NODE_ID="$2"; shift 2 ;;
            --instance-id=*) NODE_ID="${1#*=}"; shift ;;
            --instance-id)  [ "$#" -ge 2 ] || die "--instance-id requires a value"; NODE_ID="$2"; shift 2 ;;
            --secret=*)     NODE_SECRET="${1#*=}"; shift ;;
            --secret)       [ "$#" -ge 2 ] || die "--secret requires a value"; NODE_SECRET="$2"; shift 2 ;;
            --comm-mode=*)  COMM_MODE="${1#*=}"; shift ;;
            --comm-mode)    [ "$#" -ge 2 ] || die "--comm-mode requires a value"; COMM_MODE="$2"; shift 2 ;;
            --trust-keys=*) TRUST_KEYS="${1#*=}"; shift ;;
            --trust-keys)   [ "$#" -ge 2 ] || die "--trust-keys requires a value"; TRUST_KEYS="$2"; shift 2 ;;
            --dir=*)        CUSTOM_DIR="${1#*=}"; shift ;;
            --dir)          [ "$#" -ge 2 ] || die "--dir requires a value"; CUSTOM_DIR="$2"; shift 2 ;;
            --dry-run)      DRY_RUN=1; shift ;;
            -h|--help)      usage ;;
            *)
                printf '[install][error] unknown option: %s\n' "$1" >&2
                usage
                ;;
        esac
    done
}

parse_args "$@"

# ---------------------------------------------------------------------------
# environment variable fallbacks
# ---------------------------------------------------------------------------
if [ -z "$ENDPOINT" ]; then
    ENDPOINT="${ZELVIK_ENDPOINT:-${API_ENDPOINTS:-}}"
fi
if [ -z "$NODE_ID" ]; then
    NODE_ID="${ZELVIK_NODE_ID:-${ZELVIK_INSTANCE_ID:-}}"
fi
if [ -z "$NODE_SECRET" ]; then
    NODE_SECRET="${ZELVIK_NODE_SECRET:-${ZELVIK_INSTANCE_SECRET:-}}"
fi

# ---------------------------------------------------------------------------
# validate required parameters
# ---------------------------------------------------------------------------
[ -n "$ENDPOINT" ]    || die "--endpoint (or ZELVIK_ENDPOINT / API_ENDPOINTS) is required"
[ -n "$NODE_ID" ]     || die "--node-id (or ZELVIK_NODE_ID / ZELVIK_INSTANCE_ID) is required"
[ -n "$NODE_SECRET" ] || die "--secret (or ZELVIK_NODE_SECRET / ZELVIK_INSTANCE_SECRET) is required"

case "$COMM_MODE" in
    plaintext|tls|mtls) ;;
    *) die "invalid --comm-mode '$COMM_MODE': must be plaintext, tls, or mtls" ;;
esac

# ---------------------------------------------------------------------------
# root check (skip only for dry-run)
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 0 ]; then
    [ "$(id -u)" -eq 0 ] || die "must be run as root (use sudo)"
fi

# ---------------------------------------------------------------------------
# apply --dir override
# ---------------------------------------------------------------------------
if [ -n "$CUSTOM_DIR" ]; then
    CONFIG_DIR="${CUSTOM_DIR}/etc/zelvik/edge"
    DATA_DIR="${CUSTOM_DIR}/var/lib/zelvik/edge"
    LOG_DIR="${CUSTOM_DIR}/var/log/zelvik"
    INSTALL_BIN_DIR="${CUSTOM_DIR}/usr/bin"
    UNIT_DIR="${CUSTOM_DIR}/etc/systemd/system"
fi

ENV_FILE="${CONFIG_DIR}/secrets.env"
RUNTIME_FILE="${CONFIG_DIR}/runtime.yaml"

# ---------------------------------------------------------------------------
# dry-run helper
# ---------------------------------------------------------------------------
run_cmd() {
    if [ "$DRY_RUN" -eq 1 ]; then
        log "[dry-run] $*"
    else
        "$@"
    fi
}

write_file() {
    local path="$1"
    local mode="$2"
    shift 2
    if [ "$DRY_RUN" -eq 1 ]; then
        log "[dry-run] write ${path} (mode ${mode})"
    else
        cat > "$path"
        chmod "$mode" "$path"
    fi
}

# ---------------------------------------------------------------------------
# locate the binary (must be next to install.sh in the extracted tar)
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_BIN="${SCRIPT_DIR}/zelvik-edge"
[ -f "$SRC_BIN" ] || die "binary not found at ${SRC_BIN} — run from inside the extracted tar"
[ -x "$SRC_BIN" ] || chmod 0755 "$SRC_BIN"

# ---------------------------------------------------------------------------
# create user (idempotent)
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 0 ]; then
    if ! id "$SERVICE_USER" >/dev/null 2>&1; then
        useradd --system --no-create-home \
            --home-dir "$DATA_DIR" \
            --shell /usr/sbin/nologin \
            "$SERVICE_USER"
        log "created user ${SERVICE_USER}"
    fi
fi

# ---------------------------------------------------------------------------
# create directories (idempotent — never touches data/)
# ---------------------------------------------------------------------------
run_cmd install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
run_cmd install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "${DATA_DIR}/geoip"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$LOG_DIR"

# ---------------------------------------------------------------------------
# install binary
# ---------------------------------------------------------------------------
run_cmd install -m 0755 -o root -g root "$SRC_BIN" "${INSTALL_BIN_DIR}/${BIN_NAME}"
log "installed binary to ${INSTALL_BIN_DIR}/${BIN_NAME}"

# ---------------------------------------------------------------------------
# install eBPF object if present in tar
# ---------------------------------------------------------------------------
EBPF_SRC="${SCRIPT_DIR}/data/zelvik-edge-xdp-ebpf.o"
if [ -f "$EBPF_SRC" ]; then
    run_cmd install -d -m 0755 -o root -g root "${DATA_DIR}/data"
    run_cmd install -m 0644 -o root -g root "$EBPF_SRC" "${DATA_DIR}/data/zelvik-edge-xdp-ebpf.o"
    log "installed eBPF object to ${DATA_DIR}/data/zelvik-edge-xdp-ebpf.o"
fi

# ---------------------------------------------------------------------------
# render secrets.env (0600, root:root; idempotent — overwrites on re-run)
# ---------------------------------------------------------------------------
run_cmd install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
if [ "$DRY_RUN" -eq 0 ]; then
    printf '# Rendered by deploy/install.sh — root only, never commit.\nAPI_ENDPOINTS=%s\nNODE_ID=%s\nNODE_SECRET=%s\nZELVIK_COMM_MODE=%s\n' \
        "$ENDPOINT" "$NODE_ID" "$NODE_SECRET" "$COMM_MODE" > "$ENV_FILE"
    if [ -n "$TRUST_KEYS" ]; then
        printf 'ZELVIK_TRUST_KEYS=%s\n' "$TRUST_KEYS" >> "$ENV_FILE"
    fi
    chmod 0600 "$ENV_FILE"
    chown root:"$SERVICE_GROUP" "$ENV_FILE"
    log "wrote ${ENV_FILE} (0600)"
else
    log "[dry-run] write ${ENV_FILE} (0600)"
fi

# ---------------------------------------------------------------------------
# render runtime.yaml if not already present (don't overwrite operator edits)
# ---------------------------------------------------------------------------
if [ ! -f "$RUNTIME_FILE" ] || [ "$DRY_RUN" -eq 1 ]; then
    if [ "$DRY_RUN" -eq 0 ]; then
        cat > "$RUNTIME_FILE" <<EOF
runtime:
  mode: standalone
cluster:
  enabled: false
  type: standalone
  name: edge-standalone-01
  namespace: default
  cache:
    localMetaDir: ${DATA_DIR}/cache-meta
    maxFastL1Bytes: 0
    sharedMaxBytes: 100GiB
    minFreeBytes: 10GiB
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
        log "wrote ${RUNTIME_FILE}"
    else
        log "[dry-run] write ${RUNTIME_FILE}"
    fi
fi

# ---------------------------------------------------------------------------
# install systemd unit
# ---------------------------------------------------------------------------
UNIT_SRC="${SCRIPT_DIR}/systemd/${SERVICE_NAME}.service"
[ -f "$UNIT_SRC" ] || die "unit file not found: ${UNIT_SRC}"
run_cmd install -m 0644 -o root -g root "$UNIT_SRC" "${UNIT_DIR}/${SERVICE_NAME}.service"

if [ "$DRY_RUN" -eq 0 ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload
        systemctl enable "$SERVICE_NAME"
        systemctl restart "$SERVICE_NAME"
        log "enabled and started ${SERVICE_NAME}"
        log "check: journalctl -u ${SERVICE_NAME} -f"
    else
        log "systemctl not found — unit installed but not started"
    fi
else
    log "[dry-run] systemctl daemon-reload && enable && restart ${SERVICE_NAME}"
fi

log "zelvik-edge ${VERSION} installed"
