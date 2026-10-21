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
#   <home>/pki                                   root:<service group> 0750
#   <home>/configs/api_node.yaml                 0640 root:<service group>
#   <home>/pki/client.key                        0640 root:<service group>
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

INSTALL_BACKUP=""
INSTALL_COMMITTED=0
SERVICE_WAS_ACTIVE=0
ROLLBACK_RUNNING=0

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
log()  { printf '[install] %s\n' "$*"; }
die()  {
    printf '[install][error] %s\n' "$*" >&2
    if [ -n "$INSTALL_BACKUP" ] && [ "$INSTALL_COMMITTED" -eq 0 ]; then
        rollback_install
    fi
    exit 1
}

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
  --tls-cert-file <path>            Client certificate; copied to <home>/pki/client.crt (mTLS only)
  --tls-key-file <path>             Client key; copied to <home>/pki/client.key (mTLS only)
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
    exit 1
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
if [ -z "$ENDPOINT" ]; then ENDPOINT=${ZELVIK_ENDPOINT:-}; fi
if [ -z "$ENDPOINT" ]; then ENDPOINT=${API_ENDPOINTS:-}; fi
if [ -z "$NODE_ID" ]; then NODE_ID=${ZELVIK_NODE_ID:-}; fi
if [ -z "$NODE_ID" ]; then NODE_ID=${ZELVIK_INSTANCE_ID:-}; fi
if [ -z "$NODE_SECRET" ]; then NODE_SECRET=${ZELVIK_NODE_SECRET:-}; fi
if [ -z "$NODE_SECRET" ]; then NODE_SECRET=${ZELVIK_INSTANCE_SECRET:-}; fi
if [ -z "$TRUST_KEYS" ]; then TRUST_KEYS=${ZELVIK_EDGE_TRUST_KEYS:-}; fi
if [ -n "$TRUST_KEYS" ]; then
    printf '%s\n' "$TRUST_KEYS" | tr ',' '\n' | while IFS= read -r _key; do
        [ "$(printf '%s' "$_key" | wc -c | tr -d ' ')" -eq 64 ] || die "each --trust-keys entry must be 32 bytes in hex"
        case "$_key" in *[!0-9a-fA-F]*) die "--trust-keys is not hex" ;; esac
    done || exit 1
fi

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

if [ "$DRY_RUN" -eq 0 ]; then
    [ "$(id -u)" -eq 0 ] || die "must be run as root (use sudo)"
fi

# ---------------------------------------------------------------------------
# locate the binary and package metadata
# ---------------------------------------------------------------------------
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
SRC_BIN="$SCRIPT_DIR/zelvik-edge"
UNIT_SRC="$SCRIPT_DIR/systemd/$SERVICE_NAME.service"

if [ -f "$SCRIPT_DIR/VERSION" ]; then
    VERSION=$(sed -n '1{s/[[:space:]]*$//;p;}' "$SCRIPT_DIR/VERSION")
    [ -n "$VERSION" ] || die "empty VERSION file in the release package"
fi

# ---------------------------------------------------------------------------
# resolve the component home
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
CA_CONFIG_PATH=""
CLIENT_CERT_PATH=""
CLIENT_KEY_PATH=""

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

# Resolve a path without requiring the target to exist (realpath on the parent).
path_id() {
    if command -v realpath >/dev/null 2>&1; then
        realpath "$1" 2>/dev/null && return 0
    fi
    _dir=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P) || return 1
    printf '%s/%s\n' "$_dir" "$(basename "$1")"
}

same_file() {
    [ -e "$1" ] && [ -e "$2" ] || return 1
    _a=$(path_id "$1") || return 1
    _b=$(path_id "$2") || return 1
    [ "$_a" = "$_b" ]
}

# ---------------------------------------------------------------------------
# preflight: validate everything before the first write to an installed tree
# ---------------------------------------------------------------------------
preflight() {
    if [ "$DRY_RUN" -eq 0 ]; then
        command -v systemctl >/dev/null 2>&1 \
            || die "systemctl not found: cannot install and verify the service on a non-systemd host"
    fi
    [ -f "$UNIT_SRC" ] || die "unit template not found: $UNIT_SRC"
    [ -f "$SRC_BIN" ] || die "binary not found at $SRC_BIN — run from inside the extracted tar"
    if [ "$DRY_RUN" -eq 0 ] && [ ! -x "$SRC_BIN" ]; then
        chmod 0755 "$SRC_BIN"
    fi
    _candidate=$("$SRC_BIN" --version 2>/dev/null | head -n 1) || true
    [ -n "$_candidate" ] || die "candidate binary $SRC_BIN did not answer --version (wrong architecture or corrupt package)"
    if [ "$_candidate" != "zelvik-edge $VERSION" ]; then
        die "candidate binary reports '$_candidate' but the package VERSION is $VERSION; refusing to replace the installed binary"
    fi
    log "candidate binary version check: $_candidate"

    if [ -n "$TLS_CA_FILE" ] || [ -n "$TLS_CA_SHA256" ] || [ -n "$TLS_CERT_FILE" ] || [ -n "$TLS_KEY_FILE" ]; then
        command -v openssl >/dev/null 2>&1 || die "TLS validation requires openssl on the installer host"
    fi

    if [ -n "$TLS_CA_SHA256" ] && [ -z "$TLS_CA_FILE" ]; then
        die "--tls-ca-sha256 requires --tls-ca-file"
    fi
    if [ -n "$TLS_CA_FILE" ]; then
        [ -f "$TLS_CA_FILE" ] || die "--tls-ca-file not found: $TLS_CA_FILE"
        openssl x509 -in "$TLS_CA_FILE" -noout 2>/dev/null || die "cannot parse $TLS_CA_FILE as an X.509 certificate"
        if [ -n "$TLS_CA_SHA256" ]; then
            _actual=$(openssl x509 -in "$TLS_CA_FILE" -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1) || true
            [ -n "$_actual" ] || die "cannot parse $TLS_CA_FILE as an X.509 certificate"
            _expected=$(printf '%s' "$TLS_CA_SHA256" | tr 'A-F' 'a-f')
            [ "$_actual" = "$_expected" ] || die "--tls-ca-sha256 mismatch: DER sha256 of $TLS_CA_FILE is $_actual, expected $_expected"
            log "control-plane CA DER sha256 verified: $_actual"
        fi
        CA_CONFIG_PATH="$PKI_DIR/cp-ca.pem"
    elif [ -f "$PKI_DIR/cp-ca.pem" ]; then
        CA_CONFIG_PATH="$PKI_DIR/cp-ca.pem"
        log "keeping persisted control-plane CA at $CA_CONFIG_PATH"
    fi

    if [ "$COMM_MODE" != "mtls" ]; then
        if [ -n "$TLS_CERT_FILE" ] || [ -n "$TLS_KEY_FILE" ]; then
            die "--tls-cert-file/--tls-key-file require --comm-mode mtls"
        fi
        return 0
    fi

    if [ -n "$TLS_CERT_FILE" ] && [ -z "$TLS_KEY_FILE" ] || [ -z "$TLS_CERT_FILE" ] && [ -n "$TLS_KEY_FILE" ]; then
        die "--tls-cert-file and --tls-key-file must be provided together"
    fi
    if [ -n "$TLS_CERT_FILE" ]; then
        validate_client_credentials "$TLS_CERT_FILE" "$TLS_KEY_FILE"
        CLIENT_CERT_PATH="$PKI_DIR/client.crt"
        CLIENT_KEY_PATH="$PKI_DIR/client.key"
    elif [ -f "$PKI_DIR/client.crt" ] && [ -f "$PKI_DIR/client.key" ]; then
        validate_client_credentials "$PKI_DIR/client.crt" "$PKI_DIR/client.key"
        CLIENT_CERT_PATH="$PKI_DIR/client.crt"
        CLIENT_KEY_PATH="$PKI_DIR/client.key"
        log "keeping persisted mTLS client certificate at $CLIENT_CERT_PATH"
    else
        die "--comm-mode mtls requires --tls-cert-file/--tls-key-file (or previously persisted <home>/pki/client.crt and client.key)"
    fi
}

validate_client_credentials() {
    _cert="$1"
    _key="$2"
    [ -f "$_cert" ] || die "--tls-cert-file not found: $_cert"
    [ -f "$_key" ] || die "--tls-key-file not found: $_key"
    openssl x509 -in "$_cert" -noout 2>/dev/null || die "cannot parse client certificate: $_cert"
    openssl pkey -in "$_key" -noout 2>/dev/null || die "cannot parse client key: $_key"
    _cert_pub=$(openssl x509 -in "$_cert" -noout -pubkey 2>/dev/null | openssl pkey -pubin -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1)
    _key_pub=$(openssl pkey -in "$_key" -pubout -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1)
    [ -n "$_cert_pub" ] && [ "$_cert_pub" = "$_key_pub" ] || die "client certificate and key do not match"
    openssl x509 -in "$_cert" -noout -ext extendedKeyUsage 2>/dev/null \
        | grep -q "TLS Web Client Authentication" \
        || die "client certificate lacks the clientAuth extended key usage"
    _cn=$(openssl x509 -in "$_cert" -noout -subject -nameopt RFC2253 2>/dev/null \
        | sed -n 's/.*CN=\([^,]*\).*/\1/p')
    if [ -n "$_cn" ]; then
        [ "$_cn" = "$NODE_ID" ] || die "client certificate CN '$_cn' does not match --node-id '$NODE_ID'"
        case "$_cn" in
            *_*) ;;
            *) die "client certificate CN '$_cn' is not a valid instance identity" ;;
        esac
    else
        _san=$(openssl x509 -in "$_cert" -noout -ext subjectAltName 2>/dev/null | tr ',' '\n' | sed -n 's/.*DNS:\([^ ]*\).*/\1/p' | head -n 1)
        if [ -n "$_san" ]; then
            [ "$_san" = "$NODE_ID" ] || die "client certificate SAN '$_san' does not match --node-id '$NODE_ID'"
        else
            log "warning: client certificate carries no CN/SAN identity; the control plane identifies this node by instanceId metadata"
        fi
    fi
    log "mTLS client certificate validated (clientAuth, key match, identity)"
}

# ---------------------------------------------------------------------------
# backup / rollback for an installed tree. data/ is never touched.
# ---------------------------------------------------------------------------
backup_install() {
    INSTALL_BACKUP="$BACKUP_DIR/install-$(date '+%Y%m%dT%H%M%S')-$$"
    install -d -m 0700 -o root -g root "$INSTALL_BACKUP"
    backup_one bin/zelvik-edge zelvik-edge
    backup_one configs/api_node.yaml api_node.yaml
    backup_one pki/cp-ca.pem cp-ca.pem
    backup_one pki/client.crt client.crt
    backup_one pki/client.key client.key
}

backup_one() { # backup_one <relative-to-home> <backup-name>
    _src="$EDGE_HOME/$1"
    if [ -e "$_src" ]; then
        cp -a "$_src" "$INSTALL_BACKUP/$2"
        : > "$INSTALL_BACKUP/had-$2"
    fi
}

backup_path() { # backup_path <path> <backup-name>
    if [ -e "$1" ]; then
        cp -a "$1" "$INSTALL_BACKUP/$2"
        : > "$INSTALL_BACKUP/had-$2"
    fi
}

restore_home_file() { # restore_home_file <relative> <backup-name> <mode> <owner:group>
    _dst="$EDGE_HOME/$1"
    if [ -f "$INSTALL_BACKUP/had-$2" ]; then
        _tmp="$_dst.rollback.$$"
        install -m "$3" -o "${4%%:*}" -g "${4##*:}" "$INSTALL_BACKUP/$2" "$_tmp"
        mv -f "$_tmp" "$_dst"
    else
        rm -f "$_dst"
    fi
}

restore_path() { # restore_path <path> <backup-name> <mode> <owner:group>
    if [ -f "$INSTALL_BACKUP/had-$2" ]; then
        _tmp="$1.rollback.$$"
        install -m "$3" -o "${4%%:*}" -g "${4##*:}" "$INSTALL_BACKUP/$2" "$_tmp"
        mv -f "$_tmp" "$1"
    else
        rm -f "$1"
    fi
}

rollback_install() {
    [ "$ROLLBACK_RUNNING" -eq 0 ] || return 0
    ROLLBACK_RUNNING=1
    # Rollback is best-effort: one failed restore must not abort the others.
    set +e
    log "installation failed after writes; rolling back bin/config/unit/wrapper/CA (data/ preserved)"
    restore_home_file bin/zelvik-edge zelvik-edge 0755 root:root
    restore_home_file configs/api_node.yaml api_node.yaml 0640 "root:$SERVICE_GROUP"
    restore_home_file pki/cp-ca.pem cp-ca.pem 0644 root:root
    restore_home_file pki/client.crt client.crt 0644 "root:$SERVICE_GROUP"
    restore_home_file pki/client.key client.key 0640 "root:$SERVICE_GROUP"
    if [ -f "$INSTALL_BACKUP/had-xdp-object" ]; then
        install -m 0644 -o root -g root "$INSTALL_BACKUP/xdp-object" "$DATA_DIR/.xdp-object.rollback.$$"
        mv -f "$DATA_DIR/.xdp-object.rollback.$$" "$DATA_DIR/zelvik-edge-xdp-ebpf.o"
    else
        rm -f "$DATA_DIR/zelvik-edge-xdp-ebpf.o"
    fi
    if [ ! -f "$INSTALL_BACKUP/had-unit" ]; then
        systemctl disable "$SERVICE_NAME.service" >/dev/null 2>&1 || true
    fi
    restore_path "$UNIT_DST" unit 0644 root:root
    restore_path "$WRAPPER" wrapper 0755 root:root
    restore_path "/usr/bin/$BIN_NAME" legacy-bin 0755 root:root
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ "$SERVICE_WAS_ACTIVE" -eq 1 ]; then
        systemctl restart "$SERVICE_NAME.service" >/dev/null 2>&1 || true
    fi
    log "rollback complete; backup kept at $INSTALL_BACKUP"
    set -e
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

# Preflight before any write to an installed tree. A wrong CA or a package that
# cannot run must never stop the live service or overwrite its binary.
preflight

# ---------------------------------------------------------------------------
# create the layout
# ---------------------------------------------------------------------------
run_cmd install -d -m 0755 -o root -g root "$EDGE_HOME"
run_cmd install -d -m 0755 -o root -g root "$BIN_DIR"
run_cmd install -d -m 0750 -o root -g "$SERVICE_GROUP" "$CONFIG_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$DATA_DIR/geo"
run_cmd install -d -m 0750 -o "$SERVICE_USER" -g "$SERVICE_GROUP" "$LOG_DIR"
run_cmd install -d -m 0700 -o root -g root "$BACKUP_DIR"
run_cmd install -d -m 0750 -o root -g "$SERVICE_GROUP" "$PKI_DIR"
if [ ! -d "$UNIT_DIR" ]; then run_cmd install -d -m 0755 -o root -g root "$UNIT_DIR"; fi
if [ ! -d "$WRAPPER_DIR" ]; then run_cmd install -d -m 0755 -o root -g root "$WRAPPER_DIR"; fi

if [ "$DRY_RUN" -eq 0 ]; then
    chown root:root "$EDGE_HOME" "$BIN_DIR" "$BACKUP_DIR"
    chmod 0755 "$EDGE_HOME" "$BIN_DIR"
    chmod 0700 "$BACKUP_DIR"
    chown root:"$SERVICE_GROUP" "$CONFIG_DIR" "$PKI_DIR"
    chmod 0750 "$CONFIG_DIR" "$PKI_DIR"
    chown "$SERVICE_USER":"$SERVICE_GROUP" "$DATA_DIR" "$DATA_DIR/geo" "$LOG_DIR"
    chmod 0750 "$DATA_DIR" "$DATA_DIR/geo" "$LOG_DIR"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    log "[dry-run] backup bin/config/unit/wrapper, stop service, install $SRC_BIN, restart and verify"
    log "[dry-run] zelvik-edge $VERSION would be installed at $EDGE_HOME"
    exit 0
fi

# ---------------------------------------------------------------------------
# transaction: backup, stop, replace, restart, verify; die() rolls back
# ---------------------------------------------------------------------------
command -v systemctl >/dev/null 2>&1 || die "systemctl not found"
systemctl is-active --quiet "$SERVICE_NAME.service" && SERVICE_WAS_ACTIVE=1 || true
backup_install
backup_path "$UNIT_DST" unit
backup_path "$WRAPPER" wrapper
backup_path "/usr/bin/$BIN_NAME" legacy-bin
backup_path "$DATA_DIR/zelvik-edge-xdp-ebpf.o" xdp-object
log "backup written to $INSTALL_BACKUP"

if [ "$SERVICE_WAS_ACTIVE" -eq 1 ]; then
    systemctl stop "$SERVICE_NAME.service" || die "cannot stop $SERVICE_NAME.service before replacing the binary"
fi

# Binary: install to a same-directory temp then rename. Never install over a
# running binary (ETXTBSY) and never leave a half-written executable in place.
_bin_tmp="$BIN_DIR/.$BIN_NAME.install.$$"
install -m 0755 -o root -g root "$SRC_BIN" "$_bin_tmp"
_installed=$("$_bin_tmp" --version 2>/dev/null | head -n 1) || true
[ "$_installed" = "zelvik-edge $VERSION" ] || die "staged binary reports '$_installed', expected zelvik-edge $VERSION"
mv -f "$_bin_tmp" "$BIN_DIR/$BIN_NAME"
log "installed binary to $BIN_DIR/$BIN_NAME (version $VERSION)"

if [ -f "$SCRIPT_DIR/README.install" ]; then
    cp -f "$SCRIPT_DIR/README.install" "$EDGE_HOME/.README.install.$$"
    chmod 0644 "$EDGE_HOME/.README.install.$$"
    chown root:root "$EDGE_HOME/.README.install.$$"
    mv -f "$EDGE_HOME/.README.install.$$" "$EDGE_HOME/README.install"
fi

EBPF_SRC="$SCRIPT_DIR/data/zelvik-edge-xdp-ebpf.o"
if [ -f "$EBPF_SRC" ]; then
    _obj_tmp="$DATA_DIR/.zelvik-edge-xdp-ebpf.o.$$"
    install -m 0644 -o root -g root "$EBPF_SRC" "$_obj_tmp"
    mv -f "$_obj_tmp" "$DATA_DIR/zelvik-edge-xdp-ebpf.o"
    log "installed eBPF object to $DATA_DIR/zelvik-edge-xdp-ebpf.o"
fi

# CA and client credentials: persist under pki/ with temp+rename. A source that
# is already the destination (bootstrap reused the persisted CA path) is a
# no-op, not a self-copy failure.
if [ -n "$CA_CONFIG_PATH" ] && [ -n "$TLS_CA_FILE" ] && ! same_file "$TLS_CA_FILE" "$PKI_DIR/cp-ca.pem"; then
    _ca_tmp="$PKI_DIR/.cp-ca.pem.$$"
    install -m 0644 -o root -g root "$TLS_CA_FILE" "$_ca_tmp"
    mv -f "$_ca_tmp" "$PKI_DIR/cp-ca.pem"
    log "persisted control-plane CA to $PKI_DIR/cp-ca.pem"
elif same_file "$TLS_CA_FILE" "$PKI_DIR/cp-ca.pem"; then
    log "control-plane CA source is already $PKI_DIR/cp-ca.pem; no copy needed"
fi

if [ -n "$CLIENT_CERT_PATH" ]; then
    # With no --tls-cert-file/--tls-key-file the credentials are the already
    # persisted files; there is nothing to copy (and an empty source must never
    # reach install).
    if [ -n "$TLS_CERT_FILE" ] && ! same_file "$TLS_CERT_FILE" "$CLIENT_CERT_PATH"; then
        _crt_tmp="$PKI_DIR/.client.crt.$$"
        install -m 0644 -o root -g "$SERVICE_GROUP" "$TLS_CERT_FILE" "$_crt_tmp" || die "cannot persist client certificate"
        mv -f "$_crt_tmp" "$CLIENT_CERT_PATH" || die "cannot install client certificate"
    fi
    if [ -n "$TLS_KEY_FILE" ] && ! same_file "$TLS_KEY_FILE" "$CLIENT_KEY_PATH"; then
        _key_tmp="$PKI_DIR/.client.key.$$"
        install -m 0640 -o root -g "$SERVICE_GROUP" "$TLS_KEY_FILE" "$_key_tmp" || die "cannot persist client key"
        mv -f "$_key_tmp" "$CLIENT_KEY_PATH" || die "cannot install client key"
    fi
    log "persisted mTLS client certificate to $CLIENT_CERT_PATH (key 0640 root:$SERVICE_GROUP)"
fi

# Render api_node.yaml through a same-directory temp, preserving operator keys.
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
_cfg_tmp="$CONFIG_DIR/.api_node.yaml.$$"
{
    printf '# Rendered by deploy/install.sh — carries the node credential; root only.\n'
    printf 'rpc.endpoints:\n'
    printf '%s\n' "$ENDPOINT" | tr ',' '\n' | while IFS= read -r _ep; do
        if [ -n "$_ep" ]; then printf '  - "%s"\n' "$_ep"; fi
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
    if [ -n "$CA_CONFIG_PATH" ] || [ -n "$CLIENT_CERT_PATH" ] || [ -n "$TLS_SERVER_NAME" ]; then
        printf 'rpc.tls:\n'
        if [ -n "$CA_CONFIG_PATH" ]; then printf '  caFile: "%s"\n' "$CA_CONFIG_PATH"; fi
        if [ -n "$CLIENT_CERT_PATH" ]; then printf '  certFile: "%s"\n' "$CLIENT_CERT_PATH"; fi
        if [ -n "$CLIENT_KEY_PATH" ]; then printf '  keyFile: "%s"\n' "$CLIENT_KEY_PATH"; fi
        if [ -n "$TLS_SERVER_NAME" ]; then printf '  serverName: "%s"\n' "$TLS_SERVER_NAME"; fi
    fi
    if ! printf '%s\n' "$_preserved" | grep -q '^xdp:'; then
        printf 'xdp:\n  enabled: false\n'
    fi
    if [ -n "$_preserved" ]; then
        printf '\n# The keys below are not managed by the installer; it never rewrites them.\n'
        printf '%s\n' "$_preserved"
    fi
} > "$_cfg_tmp"
chmod 0640 "$_cfg_tmp"
chown root:"$SERVICE_GROUP" "$_cfg_tmp"
mv -f "$_cfg_tmp" "$API_CONFIG_FILE"
log "wrote $API_CONFIG_FILE (0640 root:$SERVICE_GROUP)"

# Wrapper and unit through same-directory temps as well.
_wrapper_tmp="$WRAPPER_DIR/.$BIN_NAME.install.$$"
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

OLD_UNIT_EXEC=""
if [ -f "$UNIT_DST" ]; then
    OLD_UNIT_EXEC=$(sed -n 's/^[[:space:]]*ExecStart=[[:space:]]*\([^[:space:]]*\).*/\1/p' "$UNIT_DST" | head -n 1)
fi
if [ "$OLD_UNIT_EXEC" = "/usr/bin/$BIN_NAME" ]; then
    rm -f "/usr/bin/$BIN_NAME"
    log "removed superseded binary /usr/bin/$BIN_NAME (unit now starts $BIN_DIR/$BIN_NAME)"
fi

_unit_tmp="$UNIT_DIR/.$SERVICE_NAME.service.$$"
sed -e "s|@ZELVIK_EDGE_HOME@|$EDGE_HOME|g" \
    -e "s|@ZELVIK_EDGE_BIN@|$BIN_DIR/$BIN_NAME|g" "$UNIT_SRC" > "$_unit_tmp"
chmod 0644 "$_unit_tmp"
chown root:root "$_unit_tmp"
mv -f "$_unit_tmp" "$UNIT_DST"
log "wrote unit $UNIT_DST (ExecStart=$BIN_DIR/$BIN_NAME)"

# ---------------------------------------------------------------------------
# start and prove it works. Any die() here rolls back the files above.
# ---------------------------------------------------------------------------
tail_journal() {
    journalctl -u "$SERVICE_NAME.service" -n 60 --no-pager >&2 || true
}

verify_installed_binary() {
    _reported=$("$BIN_DIR/$BIN_NAME" --version 2>/dev/null | head -n 1) || true
    if [ "$_reported" != "zelvik-edge $VERSION" ]; then
        tail_journal
        die "installed binary reports '$_reported' but the package VERSION is $VERSION"
    fi
    log "binary version check: $_reported"
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

verify_installed_binary
_JOURNAL_SINCE=$(date '+%Y-%m-%d %H:%M:%S')
systemctl daemon-reload
systemctl enable "$SERVICE_NAME.service" >/dev/null
systemctl restart "$SERVICE_NAME.service"
verify_running_service
verify_registration

INSTALL_COMMITTED=1
log "enabled and started $SERVICE_NAME.service"
log "check: journalctl -u $SERVICE_NAME.service -f"
log "zelvik-edge $VERSION installed at $EDGE_HOME (backup: $INSTALL_BACKUP)"
