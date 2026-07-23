#!/bin/sh
# Exercise the verified Unix Henosis installer using isolated release fixtures.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install-test.XXXXXX")
ORIGINAL_PATH=$PATH

# Remove only the temporary test tree created by this script.
cleanup() { rm -rf "$TEST_ROOT"; }

# Stop the installer contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Create one release archive whose executable records its initialization arguments.
make_release() {
    version=$1; target=$2; behavior=$3; release_dir=$4
    root="henosis-${version#v}-${target}"
    stage="$TEST_ROOT/stage-$behavior/$root"
    mkdir -p "$stage" "$release_dir/$version"
    cat > "$stage/henosis" <<EOF
#!/bin/sh
printf '%s' "\$*" > "\${HENOSIS_INIT_LOG:?}"
$behavior
EOF
    chmod 755 "$stage/henosis"
    tar -C "$(dirname "$stage")" -czf "$release_dir/$version/$root.tar.gz" "$root"
    (CDPATH= cd -- "$release_dir/$version" && sha256sum "$root.tar.gz" > SHA256SUMS)
}

# Create a curl double that serves only fixture release files.
make_curl() {
    tools=$1
    mkdir -p "$tools"
    cat > "$tools/curl" <<'EOF'
#!/bin/sh
set -eu
output=''
url=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --output) output=$2; shift 2 ;;
        *) url=$1; shift ;;
    esac
done
cp "${HENOSIS_FIXTURE_RELEASE:?}/${url##*/}" "$output"
EOF
    chmod 755 "$tools/curl"
}

# Verify a healthy installation selects the archive, verifies it, and initializes it.
test_verified_install() {
    version=v0.1.0-alpha.1; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/release"; case_root="$TEST_ROOT/success"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json"
    grep -F '"ok":true' "$case_root/result.json" >/dev/null || fail 'headless install did not report success'
    [ -x "$case_root/bin/henosis" ] || fail 'henosis was not installed'
    [ "$(cat "$case_root/init.log")" = 'init --quick' ] || fail 'installer did not run henosis init --quick'
}

# Verify a failed initializer restores the precise previous executable.
test_rollback() {
    version=v0.1.0-alpha.1; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/rollback-release"; case_root="$TEST_ROOT/rollback"
    make_release "$version" "$target" 'exit 17' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version" "$case_root/bin"
    cp "$release/$version"/* "$case_root/remote/$version/"
    printf '#!/bin/sh\nprintf previous\n' > "$case_root/bin/henosis"; chmod 755 "$case_root/bin/henosis"
    if HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then fail 'failed initialization succeeded'; fi
    [ "$("$case_root/bin/henosis")" = previous ] || fail 'previous executable was not restored'
    grep -F '"ok":false' "$case_root/result.json" >/dev/null || fail 'headless rollback did not report failure'
}

# Verify a bad mandatory checksum prevents installation.
test_checksum_failure() {
    version=v0.1.0-alpha.1; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/checksum-release"; case_root="$TEST_ROOT/checksum"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    printf '%064d  henosis-%s-%s.tar.gz\n' 0 "${version#v}" "$target" > "$case_root/remote/$version/SHA256SUMS"
    if HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then fail 'bad checksum succeeded'; fi
    [ ! -e "$case_root/bin/henosis" ] || fail 'checksum failure installed a binary'
}

trap cleanup EXIT HUP INT TERM
test_verified_install
test_rollback
test_checksum_failure
printf '%s\n' 'installer contract passed'
