#!/bin/sh
# Zelvik-Edge node installer.
#
# Unpacked from the release tar and executed by the control-plane bootstrap.
# Always run from inside an extracted tar directory; relative paths are safe.
#
# Usage (via control-plane bootstrap):
#   curl -fsSL <cp>/install/edge.sh | sudo bash -s -- \
#       --endpoint <cp-url> --node-id <id> --secret <hex> \
#       --install-dir /opt/zelvik/edge
#
# Direct invocation after unpacking:
#   sudo ./install.sh --endpoint https://cp.example.com \
#       --node-id abc123 --secret deadbeef
#
# Directory ownership (26.10.3.1):
#   <home>/bin, <home>/configs, <home>/backups   root:root
#   <home>/data, <home>/logs                     <service user>
#   <home>/configs/api_node.yaml                 0640 root:<service group>
#   <home>/pki                                   root:root, CA 0644
set -eu

# build-release.sh may overwrite this from the release argument; the package's
# VERSION file (read below) wins so the installer cannot claim a version the
# binary does not report.
VERSION="26.10.3.1"
BIN_NAME="zelvik-edge"
SERVICE_NAME="zelvik-edge"
SERVICE_USER="zelvik-edge"
SERVICE_GROUP="zelvik-edge"

DEFAULT_INSTALL_DIR="/opt/zelvik/edge"
UNIT_DIR="/etc/systemd/system"
WRAPPER_DIR="/usr/local/bin"

INSTALL_DIR=""
ENDPOINT=""
NODE_ID=""
NODE_SECRET=""
COMM_MODE="tls"
TRUST_KEYS=""
TLS_CA_FILE=""
TLS_CA_SHA256=""
TLS_CERT_FILE=""
TLS_KEY_FILE=""
TLS_SERVER_NAME=""
DRY_RUN=0
ALLOW_UNREGISTERED=0
REGISTER_TIMEOUT=60

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
  --trust-keys <hex[,hex...]>       Pinned Ed25519 geo distribution public keys
  --tls-ca-file <path>              Control-plane CA (X.509); copied to <home>/pki/cp-ca.pem
  --tls-ca-sha256 <hex>             SHA-256 of the DER encoding of that certificate
                                    (openssl x509 -outform DER | sha256sum)
  --tls-cert-file <path>            Client certificate for mTLS
  --tls-key-file <path>             Client key for mTLS
  --tls-server-name <name>          Override the TLS server name
  --install-dir <path>              Component home, not a filesystem staging prefix
                                    (default: /opt/zelvik/edge, or the home an
                                    existing unit already uses)
  --allow-unregistered              Do not fail when the node has not registered yet
                                    (offline installs; registration is retried at runtime)
  --register-timeout <seconds>      How long to wait for the first registration (default: 60)
  --dry-run                         Print actions without writing to disk or starting services

The node reads its credentials from <home>/configs/api_node.yaml; the installer
renders that file. Nothing else it writes is read by the binary.

--dir was removed: it meant "filesystem staging prefix" (a fake root), while the
node home is a single component directory. Use --install-dir <path>.

Environment variable fallbacks:
  ZELVIK_ENDPOINT or API_ENDPOINTS
  ZELVIK_NODE_ID  or ZELVIK_INSTANCE_ID
  ZELVIK_NODE_SECRET or ZELVIK_INSTANCE_SECRET
  ZELVIK_EDGE_TRUST_KEYS or --trust-keys
EOF
    exit "${1:-1}"
}

# ---------------------------------------------------------------------------
# argument parsing — accept both --flag value and --flag=value
# ---------------------------------------------------------------------------
parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --endpoint=*)   ENDPOINT=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --endpoint)     [ "$#" -ge 2 ] || die "--endpoint requires a value"; ENDPOINT="$2"; shift 2 ;;
            --node-id=*)    NODE_ID=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --node-id)      [ "$#" -ge 2 ] || die "--node-id requires a value"; NODE_ID="$2"; shift 2 ;;
            --instance-id=*) NODE_ID=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --instance-id)  [ "$#" -ge 2 ] || die "--instance-id requires a value"; NODE_ID="$2"; shift 2 ;;
            --secret=*)     NODE_SECRET=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --secret)       [ "$#" -ge 2 ] || die "--secret requires a value"; NODE_SECRET="$2"; shift 2 ;;
            --comm-mode=*)  COMM_MODE=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --comm-mode)    [ "$#" -ge 2 ] || die "--comm-mode requires a value"; COMM_MODE="$2"; shift 2 ;;
            --trust-keys=*) TRUST_KEYS=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --trust-keys)   [ "$#" -ge 2 ] || die "--trust-keys requires a value"; TRUST_KEYS="$2"; shift 2 ;;
            --tls-ca-file=*) TLS_CA_FILE=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --tls-ca-file)   [ "$#" -ge 2 ] || die "--tls-ca-file requires a value"; TLS_CA_FILE="$2"; shift 2 ;;
            --tls-ca-sha256=*) TLS_CA_SHA256=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --tls-ca-sha256)   [ "$#" -ge 2 ] || die "--tls-ca-sha256 requires a value"; TLS_CA_SHA256="$2"; shift 2 ;;
            --tls-cert-file=*) TLS_CERT_FILE=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --tls-cert-file)   [ "$#" -ge 2 ] || die "--tls-cert-file requires a value"; TLS_CERT_FILE="$2"; shift 2 ;;
            --tls-key-file=*)  TLS_KEY_FILE=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --tls-key-file)    [ "$#" -ge 2 ] || die "--tls-key-file requires a value"; TLS_KEY_FILE="$2"; shift 2 ;;
            --tls-server-name=*) TLS_SERVER_NAME=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --tls-server-name)   [ "$#" -ge 2 ] || die "--tls-server-name requires a value"; TLS_SERVER_NAME="$2"; shift 2 ;;
            --install-dir=*) INSTALL_DIR=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --install-dir)   [ "$#" -ge 2 ] || die "--install-dir requires a value"; INSTALL_DIR="$2"; shift 2 ;;
            --dir|--dir=*)
                die "--dir was removed: it was a filesystem staging prefix (fake root), while the node home is one component directory. Use --install-dir <path>." ;;
            --allow-unregistered) ALLOW_UNREGISTERED=1; shift ;;
            --register-timeout=*) REGISTER_TIMEOUT=$(printf '%s' "$1" | cut -d= -f2-); shift ;;
            --register-timeout) [ "$#" -ge 2 ] || die "--register-timeout requires a value"; REGISTER_TIMEOUT="$2"; shift 2 ;;
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
if [ -z "$ENDPOINT" ]; then ENDPOINT="${ZELVIK_ENDPOINT:-}"; fi
if [ -z "$ENDPOINT" ]; then ENDPOINT="${API_ENDPOINTS:-}"; fi
if [ -z "$NODE_ID" ]; then NODE_ID="${ZELVIK_NODE_ID:-}"; fi
if [ -z "$NODE_ID" ]; then NODE_ID="${ZELVIK_INSTANCE_ID:-}"; fi
if [ -z "$NODE_SECRET" ]; then NODE_SECRET="${ZELVIK_NODE_SECRET:-}"; fi
if [ -z "$NODE_SECRET" ]; then NODE_SECRET="${ZELVIK_INSTANCE_SECRET:-}"; fi
if [ -z "$TRUST_KEYS" ]; then TRUST_KEYS="${ZELVIK_EDGE_TRUST_KEYS:-}"; fi
if [ -n "$TRUST_KEYS" ]; then
    printf '%s\n' "$TRUST_KEYS" | tr ',' '\n' | while IFS= read -r _key; do
        [ "$(printf '%s' "$_key" | wc -c | tr -d ' ')" -eq 64 ] || die "each --trust-keys entry must be 32 bytes in hex"
        case "$_key" in *[!0-9a-fA-F]*) die "--trust-keys is not hex" ;; esac
    done || exit 1
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

case "$REGISTER_TIMEOUT" in
    ''|*[!0-9]*) die "--register-timeout must be a positive integer" ;;
esac

# ---------------------------------------------------------------------------
# root check (skip only for dry-run)
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 0 ]; then
    [ "$(id -u)" -eq 0 ] || die "must be run as root (use sudo)"
fi

# ---------------------------------------------------------------------------
# locate the binary (must be next to install.sh in the extracted tar)
# ---------------------------------------------------------------------------
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
SRC_BIN="$SCRIPT_DIR/zelvik-edge"
[ -f "$SRC_BIN" ] || die "binary not found at $SRC_BIN — run from inside the extracted tar"

# The package ships VERSION next to install.sh; it is authoritative so the
# installer cannot claim a version the binary does not report.
if [ -f "$SCRIPT_DIR/VERSION" ]; then
    VERSION=$(sed -n '1{s/[[:space:]]*$//;p;}' "$SCRIPT_DIR/VERSION")
    [ -n "$VERSION" ] || die "empty VERSION file in the release package"
fi

if [ "$DRY_RUN" -eq 0 ]; then
    [ -x "$SRC_BIN" ] || chmod 0755 "$SRC_BIN"
fi

# ---------------------------------------------------------------------------
# resolve the component home
#
# The unit is the record of an existing installation: its Environment= and
# WorkingDirectory= name the home the running node already uses. A re-run
# without --install-dir must adopt that home rather than move a live node to
# the new default; an explicit different --install-dir against an existing
# installation is refused (migration is a documented, operator-driven move).
# ---------------------------------------------------------------------------
UNIT_DST="$UNIT_DIR/$SERVICE_NAME.service"

detect_installed_home() {
    [ -f "$UNIT_DST" ] || return 1
    _home=$(sed -n 's/^[[:space:]]*Environment=.*ZELVIK_EDGE_HOME=\([^ "]*\).*/\1/p' "$UNIT_DST" | head -n 1)
    if [ -z "$_home" ]; then
        _home=$(sed -n 's/^[[:space:]]*WorkingDirectory=[[:space:]]*\([^[:space:]]*\).*/\1/p' "$UNIT_DST" | head -n 1)
    fi
    [ -n "$_home" ] || return 1
    printf '%s\n' "$_home"
}

has_existing_install() {
    _dir="$1"
    [ -n "$_dir" ] || return 1
    [ -f "$_dir/configs/api_node.yaml" ] && return 0
    [ -x "$_dir/bin/$BIN_NAME" ] && return 0
    return 1
}

EXISTING_HOME=$(detect_installed_home) || EXISTING_HOME=""

if [ -n "$INSTALL_DIR" ]; then
    EDGE_HOME="$INSTALL_DIR"
    if [ -n "$EXISTING_HOME" ] && [ "$EDGE_HOME" != "$EXISTING_HOME" ]; then
        if has_existing_install "$EXISTING_HOME"; then
            die "an installation already lives at $EXISTING_HOME (from $UNIT_DST); refusing to switch to $EDGE_HOME. Migrate explicitly: stop the service, move the home, rewrite the unit, then re-run with --install-dir. Data is never moved automatically."
        fi
    fi
else
    if [ -n "$EXISTING_HOME" ]; then
        EDGE_HOME="$EXISTING_HOME"
        log "existing installation detected at $EDGE_HOME — keeping that home (pass --install-dir to move it explicitly)"
    else
        EDGE_HOME="$DEFAULT_INSTALL_DIR"
    fi
fi

case "$EDGE_HOME" in
    ''|/) die "--install-dir must be a component directory, not '/' or empty" ;;
esac
case "$EDGE_HOME" in
    /*) ;;
    *) die "--install-dir must be an absolute path (got $EDGE_HOME)" ;;
esac
# The home is interpolated into a unit file and a shell wrapper. Accept only a
# conservative path alphabet instead of trying to quote arbitrary metacharacters.
if printf '%s' "$EDGE_HOME" | grep -q '[^A-Za-z0-9._/-]'; then
    die "--install-dir may contain only letters, digits, '.', '_', '-' and '/': $EDGE_HOME"
fi
case "/$EDGE_HOME/" in
    */../*) die "--install-dir must not contain '..' path components: $EDGE_HOME" ;;
esac

BIN_DIR="$EDGE_HOME/bin"
CONFIG_DIR="$EDGE_HOME/configs"
DATA_DIR="$EDGE_HOME/data"
LOG_DIR="$EDGE_HOME/logs"
BACKUP_DIR="$EDGE_HOME/backups"
PKI_DIR="$EDGE_HOME/pki"
API_CONFIG_FILE="$CONFIG_DIR/api_node.yaml"
WRAPPER="$WRAPPER_DIR/$BIN_NAME"

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
# create user (idempotent)
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 0 ]; then
    if ! id "$SERVICE_USER" >/dev/null 2>&1; then
        useradd --system --no-create-home \
            --home-dir "$EDGE_HOME" \
            --shell /usr/sbin/nologin \
            "$SERVICE_USER"
        log "created user $SERVICE_USER"
    fi
fi

# ---------------------------------------------------------------------------
# create the layout
#
# bin/, configs/ and backups/ are root-controlled: only the installer and a
# root operator write there. data/ and logs/ belong to the service user, and
# data/ is created unconditionally — gating it on the presence of the eBPF
# object left it root-owned on packages without data/*.o, and the node then
# failed with EACCES every time it wrote its pid file and restarted forever.
# ---------------------------------------------------------------------------
run_cmd install -d -m 0755 -o root -g root "$EDGE_HOME"
run_cmd install -d -m 0755 -o root -g root "$BIN_DIR"
run_cmd install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR/geo"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$LOG_DIR"
run_cmd install -d -m 0700 -o root -g root "$BACKUP_DIR"
run_cmd install -d -m 0755 -o root -g root "$PKI_DIR"
# /etc/systemd/system and /usr/local/bin are shared system directories: create
# them when absent, but never rewrite the owner or mode of an existing one.
if [ ! -d "$UNIT_DIR" ]; then
    run_cmd install -d -m 0755 -o root -g root "$UNIT_DIR"
fi
if [ ! -d "$WRAPPER_DIR" ]; then
    run_cmd install -d -m 0755 -o root -g root "$WRAPPER_DIR"
fi

# install -d applies the requested owner/mode only to directories it creates.
# A node installed by an older package already has data/ and logs/ on disk, so
# enforce ownership explicitly: this is what repairs an existing root-owned
# data/ instead of leaving the service to fail with EACCES on its pid file.
if [ "$DRY_RUN" -eq 0 ]; then
    chown root:root "$EDGE_HOME" "$BIN_DIR" "$BACKUP_DIR"
    chmod 0755 "$EDGE_HOME" "$BIN_DIR"
    chmod 0700 "$BACKUP_DIR"
    chown root:"$SERVICE_GROUP" "$CONFIG_DIR"
    chmod 0750 "$CONFIG_DIR"
    chown "$SERVICE_USER":"$SERVICE_GROUP" "$DATA_DIR" "$DATA_DIR/geo" "$LOG_DIR"
    chmod 0750 "$DATA_DIR" "$DATA_DIR/geo" "$LOG_DIR"
fi

# ---------------------------------------------------------------------------
# install binary into <home>/bin
# ---------------------------------------------------------------------------
if [ -f "$SCRIPT_DIR/README.install" ]; then
    run_cmd install -m 0644 -o root -g root "$SCRIPT_DIR/README.install" "$EDGE_HOME/README.install"
fi
run_cmd install -m 0755 -o root -g root "$SRC_BIN" "$BIN_DIR/$BIN_NAME"
log "installed binary to $BIN_DIR/$BIN_NAME (version $VERSION)"

# ---------------------------------------------------------------------------
# install eBPF object if present in tar (root 0644; it is read, never written)
# ---------------------------------------------------------------------------
EBPF_SRC="$SCRIPT_DIR/data/zelvik-edge-xdp-ebpf.o"
if [ -f "$EBPF_SRC" ]; then
    run_cmd install -m 0644 -o root -g root "$EBPF_SRC" "$DATA_DIR/zelvik-edge-xdp-ebpf.o"
    log "installed eBPF object to $DATA_DIR/zelvik-edge-xdp-ebpf.o"
fi

# ---------------------------------------------------------------------------
# persist the control-plane CA under <home>/pki
#
# The bootstrap hands the CA over as a temporary file. The node must not keep
# referencing that path: it disappears after the install session and the next
# restart would fail to verify the control plane. Copy it into the home, verify
# the advertised DER SHA-256 first, and point rpc.tls.caFile at the copy.
# ---------------------------------------------------------------------------
CA_CONFIG_PATH=""
if [ -n "$TLS_CA_FILE" ]; then
    [ -f "$TLS_CA_FILE" ] || die "--tls-ca-file not found: $TLS_CA_FILE"
    if [ -n "$TLS_CA_SHA256" ]; then
        command -v openssl >/dev/null 2>&1 || die "--tls-ca-sha256 requires openssl to compute the DER digest"
        _actual=$(openssl x509 -in "$TLS_CA_FILE" -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1) || die "cannot parse $TLS_CA_FILE as an X.509 certificate"
        [ -n "$_actual" ] || die "cannot parse $TLS_CA_FILE as an X.509 certificate"
        _expected=$(printf '%s' "$TLS_CA_SHA256" | tr 'A-F' 'a-f')
        [ "$_actual" = "$_expected" ] || die "--tls-ca-sha256 mismatch: DER sha256 of $TLS_CA_FILE is $_actual, expected $_expected"
        log "control-plane CA DER sha256 verified: $_actual"
    fi
    run_cmd install -m 0644 -o root -g root "$TLS_CA_FILE" "$PKI_DIR/cp-ca.pem"
    CA_CONFIG_PATH="$PKI_DIR/cp-ca.pem"
    log "persisted control-plane CA to $CA_CONFIG_PATH"
elif [ -n "$TLS_CA_SHA256" ]; then
    die "--tls-ca-sha256 requires --tls-ca-file"
fi

# A re-run without --tls-ca-file must not drop the CA the node already trusts:
# rpc.tls is installer-owned, so leaving CA_CONFIG_PATH empty would rewrite the
# section without caFile and break TLS verification on the next restart.
if [ -z "$CA_CONFIG_PATH" ] && [ -f "$PKI_DIR/cp-ca.pem" ]; then
    CA_CONFIG_PATH="$PKI_DIR/cp-ca.pem"
    log "keeping persisted control-plane CA at $CA_CONFIG_PATH"
fi

# ---------------------------------------------------------------------------
# render api_node.yaml (0640 root:<service group> — carries the credential)
#
# This is the only configuration file the node reads: both
# ApiConfig::default_paths() and RuntimeConfig::load_default() resolve
# <ZELVIK_EDGE_HOME>/configs/api_node.yaml. The installer owns exactly the
# keys below; everything else belongs to the operator and must survive a
# re-run, because clobbering it silently reverts live node settings.
# ---------------------------------------------------------------------------
_INSTALLER_KEYS='^(rpc[.]endpoints|rpc[.]tls|instanceId|secret|commMode):'
if [ -n "$TRUST_KEYS" ]; then
    _INSTALLER_KEYS='^(rpc[.]endpoints|rpc[.]tls|instanceId|secret|commMode|trustKeys):'
fi

_preserved=""
if [ -f "$API_CONFIG_FILE" ]; then
    _preserved=$(awk -v owned="$_INSTALLER_KEYS" '
        /^[^[:space:]]/ { skip = ($0 ~ owned) ? 1 : 0 }
        skip { next }
        /^# Rendered by deploy\/install\.sh/ { next }
        { print }
    ' "$API_CONFIG_FILE")
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
        if [ -n "$TRUST_KEYS" ]; then
            printf 'trustKeys:\n'
            printf '%s\n' "$TRUST_KEYS" | tr ',' '\n' | while IFS= read -r _key; do
                printf '  - "%s"\n' "$_key"
            done
        fi
        if [ -n "$CA_CONFIG_PATH" ] || [ -n "$TLS_SERVER_NAME" ] || [ "$COMM_MODE" = "mtls" ]; then
            printf 'rpc.tls:\n'
            if [ -n "$CA_CONFIG_PATH" ]; then printf '  caFile: "%s"\n' "$CA_CONFIG_PATH"; fi
            if [ -n "$TLS_CERT_FILE" ]; then printf '  certFile: "%s"\n' "$TLS_CERT_FILE"; fi
            if [ -n "$TLS_KEY_FILE" ]; then printf '  keyFile: "%s"\n' "$TLS_KEY_FILE"; fi
            if [ -n "$TLS_SERVER_NAME" ]; then printf '  serverName: "%s"\n' "$TLS_SERVER_NAME"; fi
        fi
        # XDP is opt-in per node. Only seed the default on a fresh install.
        if ! printf '%s\n' "$_preserved" | grep -q '^xdp:'; then
            printf 'xdp:\n  enabled: false\n'
        fi
        if [ -n "$_preserved" ]; then
            printf '\n# The keys below are not managed by the installer; it never rewrites them.\n'
            printf '%s\n' "$_preserved"
        fi
    } > "$API_CONFIG_FILE"
    # 0640: root owns it, the service group reads it. 0600 made the node abort
    # with EACCES on its own config.
    chmod 0640 "$API_CONFIG_FILE"
    chown root:"$SERVICE_GROUP" "$API_CONFIG_FILE"
    log "wrote $API_CONFIG_FILE (0640 root:$SERVICE_GROUP)"
else
    log "[dry-run] write $API_CONFIG_FILE (0640 root:$SERVICE_GROUP)"
fi

# ---------------------------------------------------------------------------
# install the global wrapper
#
# The wrapper pins the home: /usr/local/bin is not the node home, so without
# the export a manual invocation from /tmp would resolve the wrong configs/.
# ---------------------------------------------------------------------------
if [ "$DRY_RUN" -eq 0 ]; then
    _wrapper_tmp="$WRAPPER_DIR/.zelvik-edge.install.$$"
    {
        printf '#!/bin/sh\n'
        printf '# Installed by deploy/install.sh — pins the node home.\n'
        printf 'export ZELVIK_EDGE_HOME=%s\n' "$EDGE_HOME"
        printf 'exec %s/%s "$@"\n' "$BIN_DIR" "$BIN_NAME"
    } > "$_wrapper_tmp"
    chmod 0755 "$_wrapper_tmp"
    chown root:root "$_wrapper_tmp"
    mv -f "$_wrapper_tmp" "$WRAPPER"
    log "wrote wrapper $WRAPPER"
else
    log "[dry-run] write wrapper $WRAPPER -> $BIN_DIR/$BIN_NAME (ZELVIK_EDGE_HOME=$EDGE_HOME)"
fi

# A previous installer placed the real binary at /usr/bin/zelvik-edge and
# pointed the unit there. Once the unit points into <home>/bin that copy is a
# stale second binary; remove it only when it is the old unit's ExecStart.
OLD_UNIT_EXEC=""
if [ -f "$UNIT_DST" ]; then
    OLD_UNIT_EXEC=$(sed -n 's/^[[:space:]]*ExecStart=[[:space:]]*\([^[:space:]]*\).*/\1/p' "$UNIT_DST" | head -n 1)
fi
if [ "$OLD_UNIT_EXEC" = "/usr/bin/$BIN_NAME" ]; then
    run_cmd rm -f "/usr/bin/$BIN_NAME"
    log "removed superseded binary /usr/bin/$BIN_NAME (unit now starts $BIN_DIR/$BIN_NAME)"
fi

# ---------------------------------------------------------------------------
# install systemd unit
#
# The shipped unit is a template: @ZELVIK_EDGE_HOME@ is replaced with the
# resolved home and @ZELVIK_EDGE_BIN@ with the installed binary, so the unit,
# the wrapper and the layout can never disagree.
# ---------------------------------------------------------------------------
UNIT_SRC="$SCRIPT_DIR/systemd/$SERVICE_NAME.service"
[ -f "$UNIT_SRC" ] || die "unit file not found: $UNIT_SRC"
if [ "$DRY_RUN" -eq 0 ]; then
    sed -e "s|@ZELVIK_EDGE_HOME@|$EDGE_HOME|g" \
        -e "s|@ZELVIK_EDGE_BIN@|$BIN_DIR/$BIN_NAME|g" "$UNIT_SRC" > "$UNIT_DST"
    chmod 0644 "$UNIT_DST"
    chown root:root "$UNIT_DST"
    log "wrote unit $UNIT_DST (ExecStart=$BIN_DIR/$BIN_NAME)"
else
    log "[dry-run] render $UNIT_DST with ZELVIK_EDGE_HOME=$EDGE_HOME"
fi

# ---------------------------------------------------------------------------
# start the service and prove it works
#
# A successful systemctl restart only means systemd accepted the request. The
# node used to report success while restarting forever on EACCES. Post-start
# checks: unit active over a sustained window, a real MainPID, the installed
# binary reporting the package version, and (unless --allow-unregistered) the
# control plane having accepted this instance's registration.
# ---------------------------------------------------------------------------
verify_installed_binary() {
    _reported=$("$BIN_DIR/$BIN_NAME" --version 2>/dev/null | head -n 1) || true
    [ -n "$_reported" ] || die "installed binary $BIN_DIR/$BIN_NAME did not answer --version"
    if [ "$_reported" != "zelvik-edge $VERSION" ]; then
        die "installed binary reports '$_reported' but the package VERSION is $VERSION"
    fi
    log "binary version check: $_reported"
}

tail_journal() {
    journalctl -u "$SERVICE_NAME.service" -n 60 --no-pager >&2 || true
}

verify_running_service() {
    _i=0
    while [ "$_i" -lt 15 ]; do
        if ! systemctl is-active --quiet "$SERVICE_NAME.service"; then
            tail_journal
            die "$SERVICE_NAME.service is not active after install (state: $(systemctl is-active "$SERVICE_NAME.service" 2>/dev/null || true))"
        fi
        sleep 1
        _i=$((_i + 1))
    done
    _main_pid=$(systemctl show -p MainPID --value "$SERVICE_NAME.service" 2>/dev/null || true)
    case "$_main_pid" in
        ''|0) tail_journal; die "$SERVICE_NAME.service has no MainPID; the unit did not start a process" ;;
    esac
    log "$SERVICE_NAME.service active, MainPID=$_main_pid"
}

verify_registration() {
    if [ "$ALLOW_UNREGISTERED" -eq 1 ]; then
        log "skipping registration check (--allow-unregistered); the node retries at runtime"
        return 0
    fi
    _elapsed=0
    while [ "$_elapsed" -lt "$REGISTER_TIMEOUT" ]; do
        if journalctl -u "$SERVICE_NAME.service" --since "$_JOURNAL_SINCE" --no-pager 2>/dev/null \
            | grep -q "registered on the control channel"; then
            log "control plane accepted the instance registration"
            return 0
        fi
        sleep 2
        _elapsed=$((_elapsed + 2))
    done
    tail_journal
    die "$SERVICE_NAME.service is active but never registered within $REGISTER_TIMEOUT s; verify --endpoint and --secret, or pass --allow-unregistered for an offline install"
}

if [ "$DRY_RUN" -eq 0 ]; then
    command -v systemctl >/dev/null 2>&1 \
        || die "systemctl not found: cannot install and verify the service on a non-systemd host"
    verify_installed_binary
    _JOURNAL_SINCE=$(date '+%Y-%m-%d %H:%M:%S')
    systemctl daemon-reload
    systemctl enable "$SERVICE_NAME.service" >/dev/null
    systemctl restart "$SERVICE_NAME.service"
    verify_running_service
    verify_registration
    log "enabled and started $SERVICE_NAME.service"
    log "check: journalctl -u $SERVICE_NAME.service -f"
else
    log "[dry-run] systemctl daemon-reload && enable && restart $SERVICE_NAME.service"
fi

log "zelvik-edge $VERSION installed at $EDGE_HOME"
