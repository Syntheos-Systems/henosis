#!/bin/sh
# Exercise the Henosis installer in isolated directories with controlled tools.

set -eu

REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install-test.XXXXXX")
ORIGINAL_PATH=$PATH

# Remove only the temporary test tree created by this script.
cleanup() {
    rm -rf "$TEST_ROOT"
}

# Stop the test suite with a diagnostic.
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

# Assert that a file contains a fixed string.
assert_contains() {
    file=$1
    expected=$2
    grep -F "$expected" "$file" >/dev/null || fail "$file does not contain: $expected"
}

# Assert that a command fails.
assert_fails() {
    if "$@"; then
        fail "command unexpectedly succeeded: $*"
    fi
}

# Create the executable fixture installed by most tests.
make_fixture_binary() {
    path=$1
    printf '#!/bin/sh\nexit 0\n' > "$path"
    chmod 755 "$path"
}

# Create deterministic command doubles for Postgres and systemd.
make_tool_fixtures() {
    tools_dir=$1
    mkdir -p "$tools_dir"
    printf '#!/bin/sh\nexit 0\n' > "$tools_dir/psql"
    printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "${SYSTEMCTL_LOG:?}"\nexit 0\n' > "$tools_dir/systemctl"
    chmod 755 "$tools_dir/psql" "$tools_dir/systemctl"
}

# Run an isolated fresh install and verify its complete file contract.
test_fresh_install() {
    case_root="$TEST_ROOT/fresh install"
    fixture="$case_root/fixture-server"
    tools_dir="$case_root/tools"
    mkdir -p "$case_root"
    make_fixture_binary "$fixture"
    make_tool_fixtures "$tools_dir"
    SYSTEMCTL_LOG="$case_root/systemctl.log" PATH="$tools_dir:$ORIGINAL_PATH" \
        "$REPO_DIR/install.sh" \
        --postgres-url 'postgres://example.invalid/henosis?application_name=do-not-print' \
        --binary "$fixture" \
        --install-dir "$case_root/bin dir" \
        --config-dir "$case_root/config dir" \
        --data-dir "$case_root/data % dir" \
        --service-dir "$case_root/service % dir" \
        --no-start > "$case_root/output.log" 2>&1

    [ -x "$case_root/bin dir/syntheos-server" ] || fail "server binary was not installed"
    [ "$(stat -c '%a' "$case_root/config dir/henosis.env")" = "600" ] \
        || fail "environment file is not mode 0600"
    [ -d "$case_root/data % dir" ] || fail "data directory was not created"
    [ "$(stat -c '%a' "$case_root/config dir")" = "700" ] \
        || fail "configuration directory is not mode 0700"
    [ "$(stat -c '%a' "$case_root/data % dir")" = "700" ] \
        || fail "data directory is not mode 0700"
    [ "$(stat -c '%a' "$case_root/config dir/install-state")" = "600" ] \
        || fail "installer state is not mode 0600"
    assert_contains "$case_root/config dir/henosis.env" 'SYNTHEOS_PLUTUS_OPERATOR_TENANT='
    assert_contains "$case_root/config dir/henosis.env" 'SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL='
    assert_contains "$case_root/config dir/henosis.env" 'SYNTHEOS_PHYLAX_KEY='
    grep -Eq '^SYNTHEOS_PLUTUS_OPERATOR_TENANT=[0-9a-f-]{14}8[0-9a-f-]{21}$' \
        "$case_root/config dir/henosis.env" || fail "tenant is not UUIDv8"
    grep -Eq '^SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL=[0-9a-f-]{14}8[0-9a-f-]{21}$' \
        "$case_root/config dir/henosis.env" || fail "principal is not UUIDv8"
    grep -Eq '^SYNTHEOS_PHYLAX_KEY=[0-9a-f]{64}$' "$case_root/config dir/henosis.env" \
        || fail "Phylax key is not 32-byte hexadecimal"
    assert_contains "$case_root/service % dir/henosis.service" 'WorkingDirectory=/tmp/'
    assert_contains "$case_root/service % dir/henosis.service" 'data\x20%%\x20dir'
    assert_contains "$case_root/service % dir/henosis.service" 'EnvironmentFile=/tmp/'
    assert_contains "$case_root/service % dir/henosis.service" 'config\x20dir/henosis.env'
    if command -v systemd-analyze >/dev/null 2>&1; then
        systemd-analyze verify "$case_root/service % dir/henosis.service" \
            || fail "systemd rejected the generated service unit"
    fi
    if grep -F 'do-not-print' "$case_root/output.log" >/dev/null; then
        fail "installer printed a Postgres credential"
    fi
}

# Verify reinstallation preserves config bytes and retains the prior binary.
test_upgrade_preserves_config() {
    case_root="$TEST_ROOT/fresh install"
    before="$TEST_ROOT/config.before"
    cp "$case_root/config dir/henosis.env" "$before"
    SYSTEMCTL_LOG="$case_root/systemctl.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPO_DIR/install.sh" \
        --binary "$case_root/fixture-server" \
        --install-dir "$case_root/bin dir" \
        --config-dir "$case_root/config dir" \
        --data-dir "$case_root/ignored data dir" \
        --service-dir "$case_root/service % dir" \
        --no-start > "$case_root/upgrade.log" 2>&1
    cmp -s "$before" "$case_root/config dir/henosis.env" || fail "upgrade changed configuration"
    backups=$(find "$case_root/bin dir" -maxdepth 1 -name 'syntheos-server.bak.*' | wc -l)
    [ "$backups" -eq 1 ] || fail "upgrade did not retain exactly one prior binary"
}

# Verify input failures are nonzero and redact the supplied database URL.
test_validation_failures() {
    case_root="$TEST_ROOT/validation"
    mkdir -p "$case_root"
    make_fixture_binary "$case_root/server"
    assert_fails "$REPO_DIR/install.sh" --binary "$case_root/server" --no-service \
        > "$case_root/missing.log" 2>&1
    assert_contains "$case_root/missing.log" 'Postgres is required'
    assert_fails "$REPO_DIR/install.sh" --postgres-url 'postgres://example.invalid/db?application_name=hidden' \
        --bind 127.0.0.1:0 --binary "$case_root/server" --no-service \
        > "$case_root/bind.log" 2>&1
    assert_contains "$case_root/bind.log" 'port must be an integer from 1 through 65535'
    if grep -F 'hidden' "$case_root/bind.log" >/dev/null; then
        fail "validation printed a Postgres credential"
    fi
}

# Verify a failed live health check returns nonzero and an actionable message.
test_health_failure() {
    case_root="$TEST_ROOT/health"
    tools_dir="$case_root/tools"
    mkdir -p "$case_root"
    make_fixture_binary "$case_root/server"
    make_tool_fixtures "$tools_dir"
    printf '#!/bin/sh\nexit 22\n' > "$tools_dir/curl"
    chmod 755 "$tools_dir/curl"
    SYSTEMCTL_LOG="$case_root/systemctl.log" HENOSIS_HEALTH_ATTEMPTS=1 \
        PATH="$tools_dir:$ORIGINAL_PATH" assert_fails "$REPO_DIR/install.sh" \
        --postgres-url 'postgres://example.invalid/db?application_name=health-sentinel' \
        --binary "$case_root/server" \
        --install-dir "$case_root/bin" \
        --config-dir "$case_root/config" \
        --data-dir "$case_root/data" \
        --service-dir "$case_root/services" > "$case_root/output.log" 2>&1
    assert_contains "$case_root/output.log" 'service did not become healthy'
    if grep -F 'health-sentinel' "$case_root/output.log" >/dev/null; then
        fail "health failure printed the Postgres URL sentinel"
    fi
}

# Verify source mode performs one precise Cargo build and installs its artifact.
test_single_cargo_build() {
    case_root="$TEST_ROOT/build"
    tools_dir="$case_root/tools"
    mkdir -p "$case_root/source" "$tools_dir"
    printf '[workspace]\n' > "$case_root/source/Cargo.toml"
    make_fixture_binary "$case_root/built-server"
    make_tool_fixtures "$tools_dir"
    cat > "$tools_dir/cargo" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >> "$case_root/cargo.log"
printf '%s\n' '{"target":{"name":"syntheos-server"},"executable":"$case_root/built-server"}'
EOF
    chmod 755 "$tools_dir/cargo"
    SYSTEMCTL_LOG="$case_root/systemctl.log" PATH="$tools_dir:$ORIGINAL_PATH" \
        "$REPO_DIR/install.sh" \
        --postgres-url 'postgres://example.invalid/db' \
        --source-dir "$case_root/source" \
        --install-dir "$case_root/bin" \
        --config-dir "$case_root/config" \
        --data-dir "$case_root/data" \
        --no-service > "$case_root/output.log" 2>&1
    [ "$(wc -l < "$case_root/cargo.log")" -eq 1 ] || fail "Cargo ran more than once"
    assert_contains "$case_root/cargo.log" 'build --locked --release -p syntheos-server --bin syntheos-server'
    [ -x "$case_root/bin/syntheos-server" ] || fail "Cargo artifact was not installed"
}

trap cleanup EXIT HUP INT TERM
test_fresh_install
test_upgrade_preserves_config
test_validation_failures
test_health_failure
test_single_cargo_build
printf 'PASS: Henosis installer tests\n'
