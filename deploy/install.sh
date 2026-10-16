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
DATA_DIR="/var/lib/zelvik/edge"
LOG_DIR="/var/log/zelvik"
UNIT_DIR="/etc/systemd/system"

ENDPOINT=""
NODE_ID=""
NODE_SECRET=""
COMM_MODE="tls"
TRUST_KEYS=""
TLS_CA_FILE=""
TLS_CERT_FILE=""
TLS_KEY_FILE=""
TLS_SERVER_NAME=""
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

  --endpoint <url>                  Control-plane RPC base URL (comma-separated for multiple)
  --node-id <id>                    Node identity  (alias: --instance-id)
  --secret <hex>                    One-time registration secret
  --comm-mode <plaintext|tls|mtls>  Communication mode (default: tls)
  --trust-keys <hex[,hex...]>       Pinned trust keys (accepted, forwarded to the node)
  --tls-ca-file <path>              CA bundle for mTLS  (--comm-mode mtls)
  --tls-cert-file <path>            Client certificate for mTLS
  --tls-key-file <path>             Client key for mTLS
  --tls-server-name <name>          Override the TLS server name
  --dir <path>                      Installation root (overrides layout defaults)
  --dry-run                         Print actions without writing to disk or starting services

The node reads its credentials from <home>/configs/api_node.yaml; the installer
renders that file. Nothing else it writes is read by the binary.

Environment variable fallbacks:
  ZELVIK_ENDPOINT or API_ENDPOINTS
  ZELVIK_NODE_ID  or ZELVIK_INSTANCE_ID
  ZELVIK_NODE_SECRET or ZELVIK_INSTANCE_SECRET
EOF
    exit "${1:-1}"
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
            --tls-ca-file=*)   TLS_CA_FILE="${1#*=}"; shift ;;
            --tls-ca-file)     [ "$#" -ge 2 ] || die "--tls-ca-file requires a value"; TLS_CA_FILE="$2"; shift 2 ;;
            --tls-cert-file=*) TLS_CERT_FILE="${1#*=}"; shift ;;
            --tls-cert-file)   [ "$#" -ge 2 ] || die "--tls-cert-file requires a value"; TLS_CERT_FILE="$2"; shift 2 ;;
            --tls-key-file=*)  TLS_KEY_FILE="${1#*=}"; shift ;;
            --tls-key-file)    [ "$#" -ge 2 ] || die "--tls-key-file requires a value"; TLS_KEY_FILE="$2"; shift 2 ;;
            --tls-server-name=*) TLS_SERVER_NAME="${1#*=}"; shift ;;
            --tls-server-name)   [ "$#" -ge 2 ] || die "--tls-server-name requires a value"; TLS_SERVER_NAME="$2"; shift 2 ;;
            --dir=*)        CUSTOM_DIR="${1#*=}"; shift ;;
            --dir)          [ "$#" -ge 2 ] || die "--dir requires a value"; CUSTOM_DIR="$2"; shift 2 ;;
            --dry-run)      DRY_RUN=1; shift ;;
            -h|--help)      usage 0 ;;
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
    DATA_DIR="${CUSTOM_DIR}/var/lib/zelvik/edge"
    LOG_DIR="${CUSTOM_DIR}/var/log/zelvik"
    INSTALL_BIN_DIR="${CUSTOM_DIR}/usr/bin"
    UNIT_DIR="${CUSTOM_DIR}/etc/systemd/system"
fi

# The node resolves every runtime path (configs/, data/, logs/) relative to its
# home. ZELVIK_EDGE_HOME is the only name the binary reads — see
# src/paths.rs NodePaths::current(). The legacy CLOUD_NODE_HOME and
# ZELVIK_CONFIG_DIR names appear nowhere in src/, so the unit no longer sets them.
EDGE_HOME="$DATA_DIR"
API_CONFIG_FILE="${EDGE_HOME}/configs/api_node.yaml"

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

# ---------------------------------------------------------------------------
# locate the binary (must be next to install.sh in the extracted tar)
# ---------------------------------------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_BIN="${SCRIPT_DIR}/zelvik-edge"
[ -f "$SRC_BIN" ] || die "binary not found at ${SRC_BIN} — run from inside the extracted tar"
if [ "$DRY_RUN" -eq 0 ]; then
    [ -x "$SRC_BIN" ] || chmod 0755 "$SRC_BIN"
fi

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
#
# There is no /etc/zelvik/edge: the node reads nothing from it, and a directory
# that looks configured while nothing consumes it is how the previous install
# path ended up writing files the binary never opened.
# ---------------------------------------------------------------------------
run_cmd install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "${DATA_DIR}/geoip"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$LOG_DIR"
# These two live under a path that always exists on a default install (/usr/bin,
# /etc/systemd/system) but not under --dir; without them the binary and the unit
# cannot be written at all.
run_cmd install -d -m 0755 -o root -g root "$INSTALL_BIN_DIR"
run_cmd install -d -m 0755 -o root -g root "$UNIT_DIR"

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
# render api_node.yaml (0600 — carries the node credential)
#
# This is the only configuration file the node reads: both
# ApiConfig::default_paths() and RuntimeConfig::load_default() resolve
# <ZELVIK_EDGE_HOME>/configs/api_node.yaml. The installer previously wrote
# secrets.env and runtime.yaml here instead; nothing in src/ ever read either,
# so a freshly installed node could not start. Do not reintroduce them.
# ---------------------------------------------------------------------------
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "${EDGE_HOME}/configs"

# The installer owns exactly these top-level keys. Everything else in the file
# belongs to the operator (kernelTuning, cache.sendfile, gcloudNodeId, relay,
# accessLogPipeline, rpc.disableUpdate, xdp, ...) and must survive a re-run:
# this is the only file the node reads, so clobbering it silently reverts live
# node settings on every upgrade.
_INSTALLER_KEYS='^(rpc\.endpoints|rpc\.tls|instanceId|secret|commMode):'

# Keep every line except the installer-owned keys and their indented children,
# plus the installer's own header so a re-run does not stack copies of it.
_preserved=""
if [ -f "$API_CONFIG_FILE" ]; then
    _preserved="$(awk -v owned="$_INSTALLER_KEYS" '
        /^[^[:space:]]/ { skip = ($0 ~ owned) ? 1 : 0 }
        skip { next }
        /^# Rendered by deploy\/install\.sh/ { next }
        { print }
    ' "$API_CONFIG_FILE")"
fi

if [ "$DRY_RUN" -eq 0 ]; then
    {
        printf '# Rendered by deploy/install.sh — carries the node credential; root only.\n'
        printf 'rpc.endpoints:\n'
        printf '%s\n' "$ENDPOINT" | tr ',' '\n' | while IFS= read -r _ep; do
            if [ -n "$_ep" ]; then
                printf '  - "%s"\n' "$_ep"
            fi
        done
        printf 'instanceId: "%s"\n' "$NODE_ID"
        printf 'secret: "%s"\n' "$NODE_SECRET"
        printf 'commMode: "%s"\n' "$COMM_MODE"
        if [ "$COMM_MODE" = "mtls" ]; then
            printf 'rpc.tls:\n'
            if [ -n "$TLS_CA_FILE" ]; then printf '  caFile: "%s"\n' "$TLS_CA_FILE"; fi
            if [ -n "$TLS_CERT_FILE" ]; then printf '  certFile: "%s"\n' "$TLS_CERT_FILE"; fi
            if [ -n "$TLS_KEY_FILE" ]; then printf '  keyFile: "%s"\n' "$TLS_KEY_FILE"; fi
            if [ -n "$TLS_SERVER_NAME" ]; then printf '  serverName: "%s"\n' "$TLS_SERVER_NAME"; fi
        fi
        # XDP is opt-in per node. Only seed the default on a fresh install: once
        # the operator has set it, that value is theirs and is preserved below.
        if ! printf '%s\n' "$_preserved" | grep -q '^xdp:'; then
            printf 'xdp:\n  enabled: false\n'
        fi
        if [ -n "$_preserved" ]; then
            printf '\n# The keys below are not managed by the installer; it never rewrites them.\n'
            printf '%s\n' "$_preserved"
        fi
    } > "$API_CONFIG_FILE"
    # 0640, not 0600: the file is root-owned and the service runs as
    # zelvik-edge, so the owner bits never apply to the reader. 0600 made the
    # node abort with EACCES on its own config.
    chmod 0640 "$API_CONFIG_FILE"
    chown root:"$SERVICE_GROUP" "$API_CONFIG_FILE"
    log "wrote ${API_CONFIG_FILE} (0640)"
else
    log "[dry-run] write ${API_CONFIG_FILE} (0640)"
fi

# ---------------------------------------------------------------------------
# install systemd unit
#
# The shipped unit is a template: @ZELVIK_EDGE_HOME@ is replaced with the
# resolved node home so `--dir` overrides stay consistent between the unit and
# the config tree it reads.
# ---------------------------------------------------------------------------
UNIT_SRC="${SCRIPT_DIR}/systemd/${SERVICE_NAME}.service"
[ -f "$UNIT_SRC" ] || die "unit file not found: ${UNIT_SRC}"
UNIT_DST="${UNIT_DIR}/${SERVICE_NAME}.service"
if [ "$DRY_RUN" -eq 0 ]; then
    sed -e "s|@ZELVIK_EDGE_HOME@|${EDGE_HOME}|g" \
        -e "s|@ZELVIK_EDGE_BIN@|${INSTALL_BIN_DIR}/${BIN_NAME}|g" "$UNIT_SRC" > "$UNIT_DST"
    chmod 0644 "$UNIT_DST"
    chown root:root "$UNIT_DST"
else
    log "[dry-run] render ${UNIT_DST} with ZELVIK_EDGE_HOME=${EDGE_HOME}"
fi

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
