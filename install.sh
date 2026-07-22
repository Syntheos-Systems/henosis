#!/bin/sh
# Install Henosis from this checkout or from a supplied syntheos-server binary.

set -eu

PROGRAM="henosis-installer"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
INSTALL_DIR=${HENOSIS_INSTALL_DIR:-"${HOME}/.local/bin"}
CONFIG_DIR=${HENOSIS_CONFIG_DIR:-"${XDG_CONFIG_HOME:-${HOME}/.config}/henosis"}
DATA_DIR=${HENOSIS_DATA_DIR:-"${XDG_DATA_HOME:-${HOME}/.local/share}/henosis"}
SERVICE_DIR=${HENOSIS_SERVICE_DIR:-"${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"}
SOURCE_DIR=$SCRIPT_DIR
SOURCE_BINARY=""
POSTGRES_URL=${SYNTHEOS_PLUTUS_DB:-}
BIND_ADDR=${SYNTHEOS_ADDR:-127.0.0.1:8088}
INSTALL_SERVICE=1
START_SERVICE=1
HEALTH_ATTEMPTS=${HENOSIS_HEALTH_ATTEMPTS:-30}
BUILD_LOG=""
PROMPT_STTY=""

# Print one informational line without exposing configuration values.
info() {
    printf '%s: %s\n' "$PROGRAM" "$*"
}

# Print an error and terminate the installer.
die() {
    printf '%s: error: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

# Display the supported installer interface.
usage() {
    cat <<'EOF'
Usage: ./install.sh [--postgres-url URL] [options]

Install the integrated Henosis server from the current source checkout.

Database:
  On a fresh interactive install, the Postgres URL is requested without echo.
  For automation, pass --postgres-url URL or set SYNTHEOS_PLUTUS_DB.

Options:
  --binary PATH         Install a prebuilt syntheos-server instead of building.
  --source-dir PATH     Source checkout to build (default: installer directory).
  --install-dir PATH    Binary directory (default: ~/.local/bin).
  --config-dir PATH     Private configuration directory.
  --data-dir PATH       Persistent SQLite data directory.
  --service-dir PATH    systemd user unit directory.
  --bind ADDRESS        Listen address (default: 127.0.0.1:8088).
  --no-service          Do not install or control a systemd user service.
  --no-start            Install the service but do not start it.
  -h, --help            Show this help.

Existing configuration is preserved exactly. Re-running the installer updates
the binary and service definition without rotating identities or secrets.
EOF
}

# Require a value after an option and expose it through OPTION_VALUE.
take_value() {
    option=$1
    remaining=$2
    [ "$remaining" -ge 2 ] || die "$option requires a value"
    OPTION_VALUE=$3
}

# Parse command-line options into installer state.
parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --postgres-url|--binary|--source-dir|--install-dir|--config-dir|--data-dir|--service-dir|--bind)
                take_value "$1" "$#" "${2-}"
                case "$1" in
                    --postgres-url) POSTGRES_URL=$OPTION_VALUE ;;
                    --binary) SOURCE_BINARY=$OPTION_VALUE ;;
                    --source-dir) SOURCE_DIR=$OPTION_VALUE ;;
                    --install-dir) INSTALL_DIR=$OPTION_VALUE ;;
                    --config-dir) CONFIG_DIR=$OPTION_VALUE ;;
                    --data-dir) DATA_DIR=$OPTION_VALUE ;;
                    --service-dir) SERVICE_DIR=$OPTION_VALUE ;;
                    --bind) BIND_ADDR=$OPTION_VALUE ;;
                esac
                shift 2
                ;;
            --no-service)
                INSTALL_SERVICE=0
                START_SERVICE=0
                shift
                ;;
            --no-start)
                START_SERVICE=0
                shift
                ;;
            -h|--help)
                usage
                exit 0
                ;;
            *) die "unknown option: $1" ;;
        esac
    done
}

# Reject values that cannot be represented safely in generated files.
validate_single_line() {
    label=$1
    value=$2
    carriage_return=$(printf '\r')
    case "$value" in
        *'
'*) die "$label must be a single line" ;;
        *"$carriage_return"*) die "$label must be a single line" ;;
    esac
}

# Validate the supported host:port listen form and reject an unusable port.
validate_bind_addr() {
    validate_single_line "bind address" "$BIND_ADDR"
    case "$BIND_ADDR" in
        *:*) ;;
        *) die "bind address must use host:port form" ;;
    esac
    bind_host=${BIND_ADDR%:*}
    case "$bind_host" in
        ''|*[!A-Za-z0-9_.:\[\]-]*) die "bind address contains unsupported host characters" ;;
    esac
    bind_port=${BIND_ADDR##*:}
    case "$bind_port" in
        ''|*[!0-9]*) die "bind address port must be an integer from 1 through 65535" ;;
    esac
    [ "$bind_port" -ge 1 ] 2>/dev/null && [ "$bind_port" -le 65535 ] 2>/dev/null \
        || die "bind address port must be an integer from 1 through 65535"
}

# Convert a path to an absolute path after creating its directory.
prepare_dir() {
    path=$1
    mkdir -p "$path"
    (CDPATH= cd -- "$path" && pwd -P)
}

# Generate a UUID with RFC 9562 version and variant bits suitable for Henosis IDs.
generate_uuid_v8() {
    raw=$(openssl rand -hex 16) || die "OpenSSL could not generate a UUID"
    printf '%s-%s-8%s-8%s-%s\n' \
        "$(printf '%s' "$raw" | cut -c1-8)" \
        "$(printf '%s' "$raw" | cut -c9-12)" \
        "$(printf '%s' "$raw" | cut -c14-16)" \
        "$(printf '%s' "$raw" | cut -c18-20)" \
        "$(printf '%s' "$raw" | cut -c21-32)"
}

# Escape a value for systemd EnvironmentFile double-quoted syntax.
quote_env_value() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# Restore terminal settings after a hidden interactive prompt.
restore_terminal() {
    if [ -n "$PROMPT_STTY" ]; then
        stty "$PROMPT_STTY" < /dev/tty 2>/dev/null || true
        PROMPT_STTY=""
    fi
}

# Read the required Postgres URL without echoing it or placing it in shell history.
prompt_postgres_url() {
    [ -t 0 ] || die "Postgres is required; pass --postgres-url or set SYNTHEOS_PLUTUS_DB"
    PROMPT_STTY=$(stty -g < /dev/tty) || die "could not read terminal settings"
    stty -echo < /dev/tty || die "could not disable terminal echo"
    printf 'Postgres URL for the Plutus authority: ' > /dev/tty
    if ! IFS= read -r POSTGRES_URL < /dev/tty; then
        restore_terminal
        die "could not read the Postgres URL"
    fi
    restore_terminal
    printf '\n' > /dev/tty
}

# Write the initial owner-only runtime environment without printing secrets.
write_initial_env() {
    env_path=$1
    env_tmp=$(mktemp "${CONFIG_DIR}/.henosis.env.XXXXXX") \
        || die "could not create a private configuration file"
    chmod 600 "$env_tmp"
    tenant=$(generate_uuid_v8)
    principal=$(generate_uuid_v8)
    phylax_key=$(openssl rand -hex 32) || die "OpenSSL could not generate the Phylax key"
    postgres_escaped=$(quote_env_value "$POSTGRES_URL")
    {
        printf '# Generated by the Henosis installer. Keep this file private.\n'
        printf 'RUST_LOG=info\n'
        printf 'SYNTHEOS_ADDR=%s\n' "$BIND_ADDR"
        printf 'SYNTHEOS_PLUTUS_DB="%s"\n' "$postgres_escaped"
        printf 'SYNTHEOS_PLUTUS_OPERATOR_TENANT=%s\n' "$tenant"
        printf 'SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL=%s\n' "$principal"
        printf 'SYNTHEOS_PHYLAX_KEY=%s\n' "$phylax_key"
        printf 'SYNTHEOS_IDENTITY_DB="%s/identity.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_CHIASM_DB="%s/chiasm.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_SOMA_DB="%s/soma.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_BROCA_DB="%s/broca.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_LOOM_DB="%s/loom.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_THYMUS_DB="%s/thymus.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
        printf 'SYNTHEOS_PHYLAX_DB="%s/phylax.sqlite"\n' "$(quote_env_value "$DATA_DIR")"
    } > "$env_tmp"
    mv "$env_tmp" "$env_path"
    chmod 600 "$env_path"
}

# Record the data directory needed by the service sandbox without storing secrets.
write_install_state() {
    state_path=$1
    state_tmp=$(mktemp "${CONFIG_DIR}/.install-state.XXXXXX") \
        || die "could not create installer state"
    chmod 600 "$state_tmp"
    printf 'DATA_DIR=%s\n' "$DATA_DIR" > "$state_tmp"
    mv "$state_tmp" "$state_path"
    chmod 600 "$state_path"
}

# Recover the original data directory without executing installer state as shell code.
configured_data_dir() {
    state_path=$1
    configured=$(sed -n 's/^DATA_DIR=//p' "$state_path" | tail -n 1)
    [ -n "$configured" ] || die "installer state has no DATA_DIR"
    printf '%s\n' "$configured"
}

# Read the unquoted bind address generated by this installer without sourcing the file.
configured_bind_addr() {
    env_path=$1
    configured=$(sed -n 's/^SYNTHEOS_ADDR=//p' "$env_path" | tail -n 1)
    [ -n "$configured" ] || die "existing configuration has no SYNTHEOS_ADDR"
    printf '%s\n' "$configured"
}

# Verify that the supplied Postgres endpoint is usable when psql is available.
check_postgres() {
    if ! command -v psql >/dev/null 2>&1; then
        info "psql not found; the server will perform the Postgres connection check at startup"
        return
    fi
    if ! PGCONNECT_TIMEOUT=5 PGDATABASE="$POSTGRES_URL" psql -v ON_ERROR_STOP=1 -Atqc 'SELECT 1' \
        >/dev/null 2>&1; then
        die "could not connect to the configured Postgres database"
    fi
    info "Postgres connection verified"
}

# Build syntheos-server once and return the executable path emitted by Cargo.
build_server() {
    command -v cargo >/dev/null 2>&1 || die "cargo is required when --binary is not supplied"
    [ -f "$SOURCE_DIR/Cargo.toml" ] || die "source directory has no Cargo.toml"
    BUILD_LOG=$(mktemp "${TMPDIR:-/tmp}/henosis-build.XXXXXX") \
        || die "could not create a build log"
    info "building syntheos-server from the current checkout"
    if ! (CDPATH= cd -- "$SOURCE_DIR" && cargo build --locked --release -p syntheos-server \
        --bin syntheos-server --message-format=json-render-diagnostics) > "$BUILD_LOG"; then
        die "Cargo build failed"
    fi
    built=$(sed -n '/"name":"syntheos-server"/s/.*"executable":"\([^"]*\)".*/\1/p' "$BUILD_LOG" | tail -n 1)
    [ -n "$built" ] && [ -x "$built" ] || die "Cargo did not report a syntheos-server executable"
    SOURCE_BINARY=$built
}

# Install a binary atomically and retain a timestamped copy of the prior version.
install_binary() {
    [ -f "$SOURCE_BINARY" ] || die "binary not found: $SOURCE_BINARY"
    [ -x "$SOURCE_BINARY" ] || die "binary is not executable: $SOURCE_BINARY"
    destination="$INSTALL_DIR/syntheos-server"
    binary_tmp=$(mktemp "${INSTALL_DIR}/.syntheos-server.XXXXXX") \
        || die "could not create a temporary binary"
    cp "$SOURCE_BINARY" "$binary_tmp"
    chmod 755 "$binary_tmp"
    if [ -e "$destination" ]; then
        backup=$(mktemp "${destination}.bak.$(date -u +%Y%m%dT%H%M%SZ).XXXXXX") \
            || die "could not reserve a binary backup path"
        cp -p "$destination" "$backup"
        info "preserved the previous binary at $backup"
    fi
    mv "$binary_tmp" "$destination"
    INSTALLED_BINARY=$destination
}

# Escape a path for systemd directives that do not accept shell-style quotes.
escape_systemd_path() {
    tab=$(printf '\t')
    printf '%s' "$1" | sed \
        -e 's/\\/\\x5c/g' \
        -e 's/%/%%/g' \
        -e 's/ /\\x20/g' \
        -e "s/$tab/\\\\x09/g" \
        -e 's/"/\\x22/g' \
        -e "s/'/\\\\x27/g" \
        -e 's/#/\\x23/g'
}

# Write the systemd user unit atomically, backing up a prior definition.
install_systemd_unit() {
    command -v systemctl >/dev/null 2>&1 || die "systemctl is unavailable; use --no-service"
    if ! systemctl --user show-environment >/dev/null 2>&1; then
        die "the systemd user manager is unavailable; use --no-service"
    fi
    unit_path="$SERVICE_DIR/henosis.service"
    unit_tmp=$(mktemp "${SERVICE_DIR}/.henosis.service.XXXXXX") \
        || die "could not create a temporary service unit"
    {
        printf '[Unit]\n'
        printf 'Description=Henosis agent runtime\n'
        printf 'Documentation=https://github.com/Syntheos-Systems/henosis\n'
        printf 'Wants=network-online.target\n'
        printf 'After=network-online.target\n\n'
        printf '[Service]\n'
        printf 'Type=simple\n'
        printf 'WorkingDirectory=%s\n' "$(escape_systemd_path "$DATA_DIR")"
        printf 'ExecStart=%s\n' "$(escape_systemd_path "$INSTALLED_BINARY")"
        printf 'EnvironmentFile=%s\n' "$(escape_systemd_path "$CONFIG_DIR/henosis.env")"
        printf 'Restart=on-failure\n'
        printf 'RestartSec=5s\n'
        printf 'NoNewPrivileges=true\n'
        printf 'PrivateTmp=true\n'
        printf 'ProtectSystem=strict\n'
        printf 'ProtectHome=read-only\n'
        printf 'ReadWritePaths=%s\n\n' "$(escape_systemd_path "$DATA_DIR")"
        printf '[Install]\n'
        printf 'WantedBy=default.target\n'
    } > "$unit_tmp"
    if [ -e "$unit_path" ] && ! cmp -s "$unit_tmp" "$unit_path"; then
        backup=$(mktemp "${unit_path}.bak.$(date -u +%Y%m%dT%H%M%SZ).XXXXXX") \
            || die "could not reserve a service backup path"
        cp -p "$unit_path" "$backup"
        info "preserved the previous service unit at $backup"
    fi
    mv "$unit_tmp" "$unit_path"
    systemctl --user daemon-reload
}

# Convert the configured listen address into a local health-check URL.
health_url() {
    addr=$1
    port=${addr##*:}
    host=${addr%:*}
    case "$host" in
        0.0.0.0|'[::]'|'::') host=127.0.0.1 ;;
    esac
    printf 'http://%s:%s/health\n' "$host" "$port"
}

# Probe the server once with curl or wget.
probe_health() {
    url=$1
    if command -v curl >/dev/null 2>&1; then
        [ "$(curl -fsS --max-time 2 "$url" 2>/dev/null || true)" = "ok" ]
    elif command -v wget >/dev/null 2>&1; then
        [ "$(wget -qO- -T 2 "$url" 2>/dev/null || true)" = "ok" ]
    else
        die "curl or wget is required to verify the running service"
    fi
}

# Wait for the real Henosis health endpoint after service startup.
wait_for_health() {
    url=$(health_url "$BIND_ADDR")
    attempt=1
    while [ "$attempt" -le "$HEALTH_ATTEMPTS" ]; do
        if probe_health "$url"; then
            info "health check passed at $url"
            return
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    systemctl --user status henosis.service --no-pager >&2 || true
    die "service did not become healthy at $url"
}

# Remove only installer-owned temporary build output records.
cleanup() {
    restore_terminal
    if [ -n "$BUILD_LOG" ] && [ -f "$BUILD_LOG" ]; then
        rm -f "$BUILD_LOG"
    fi
}

# Execute the ordered installation transaction.
main() {
    parse_args "$@"
    [ "$(uname -s)" = "Linux" ] || die "this installer currently supports Linux only"
    command -v openssl >/dev/null 2>&1 || die "openssl is required for secure identity and key generation"
    validate_single_line "Postgres URL" "$POSTGRES_URL"
    validate_single_line "install directory" "$INSTALL_DIR"
    validate_single_line "configuration directory" "$CONFIG_DIR"
    validate_single_line "data directory" "$DATA_DIR"
    validate_single_line "service directory" "$SERVICE_DIR"
    validate_bind_addr
    case "$HEALTH_ATTEMPTS" in
        ''|*[!0-9]*) die "HENOSIS_HEALTH_ATTEMPTS must be a positive integer" ;;
    esac
    [ "$HEALTH_ATTEMPTS" -ge 1 ] || die "HENOSIS_HEALTH_ATTEMPTS must be a positive integer"

    INSTALL_DIR=$(prepare_dir "$INSTALL_DIR")
    CONFIG_DIR=$(prepare_dir "$CONFIG_DIR")
    chmod 700 "$CONFIG_DIR"
    env_path="$CONFIG_DIR/henosis.env"
    state_path="$CONFIG_DIR/install-state"
    if [ -e "$env_path" ]; then
        [ -f "$env_path" ] && [ ! -L "$env_path" ] || die "existing configuration is not a regular file"
        [ -f "$state_path" ] && [ ! -L "$state_path" ] \
            || die "existing configuration has no trusted installer state; move it aside or use another --config-dir"
        BIND_ADDR=$(configured_bind_addr "$env_path")
        DATA_DIR=$(configured_data_dir "$state_path")
        validate_single_line "stored data directory" "$DATA_DIR"
        validate_bind_addr
        chmod 600 "$env_path" "$state_path"
        info "preserving existing configuration and authority secrets"
    else
        if [ -z "$POSTGRES_URL" ]; then
            prompt_postgres_url
        fi
        validate_single_line "Postgres URL" "$POSTGRES_URL"
        DATA_DIR=$(prepare_dir "$DATA_DIR")
        check_postgres
        write_initial_env "$env_path"
        write_install_state "$state_path"
        info "created private runtime configuration at $env_path"
    fi
    DATA_DIR=$(prepare_dir "$DATA_DIR")
    chmod 700 "$DATA_DIR"
    if [ "$INSTALL_SERVICE" -eq 1 ]; then
        SERVICE_DIR=$(prepare_dir "$SERVICE_DIR")
    fi

    if [ -z "$SOURCE_BINARY" ]; then
        build_server
    fi
    install_binary
    info "installed $INSTALLED_BINARY"

    if [ "$INSTALL_SERVICE" -eq 1 ]; then
        install_systemd_unit
        if [ "$START_SERVICE" -eq 1 ]; then
            systemctl --user enable --now henosis.service
            wait_for_health
        else
            info "service installed but not started (--no-start)"
        fi
    else
        info "service integration skipped (--no-service)"
        info "run with: set -a; . $env_path; set +a; $INSTALLED_BINARY"
    fi

    info "installation complete"
}

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
main "$@"
