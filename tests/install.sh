#!/bin/sh
# Exercise the verified Unix Henosis installer using isolated release fixtures.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install-test.XXXXXX")
ORIGINAL_PATH=$PATH

# These fixtures are synthetic releases served through stubbed download tooling,
# so they carry no sigstore attestation. Without this the installer would reach
# out to GitHub on every case and this suite would depend on the network.
HENOSIS_SKIP_ATTESTATION=1
export HENOSIS_SKIP_ATTESTATION

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
    printf '#!/bin/sh\nexit 0\n' > "$stage/crucible"
    chmod 755 "$stage/henosis" "$stage/crucible"
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
case "$url" in
    */releases/tags/*)
        case "${HENOSIS_FIXTURE_METADATA:-valid}" in
            valid)
                printf '{"tag_name":"%s","draft":false,"immutable":%s}\n' \
                    "${url##*/}" "${HENOSIS_FIXTURE_IMMUTABLE:-true}" > "$output"
                ;;
            embedded-true)
                printf '{"tag_name":"%s","draft":false,"immutable":false,"body":"\\"immutable\\":true"}\n' \
                    "${url##*/}" > "$output"
                ;;
            nested-true)
                printf '{"tag_name":"%s","draft":false,"immutable":false,"nested":{"immutable":true}}\n' \
                    "${url##*/}" > "$output"
                ;;
            duplicate)
                printf '{"tag_name":"%s","tag_name":"%s","draft":false,"immutable":true}\n' \
                    "${url##*/}" "${url##*/}" > "$output"
                ;;
            malformed)
                printf '{"tag_name":"%s","draft":false,"immutable":true\n' "${url##*/}" > "$output"
                ;;
            malformed-token)
                printf '{"tag_name":"%s","draft":false garbage,"immutable":true}\n' \
                    "${url##*/}" > "$output"
                ;;
            malformed-nested)
                printf '{"tag_name":"%s","draft":false,"immutable":true,"nested":[true false]}\n' \
                    "${url##*/}" > "$output"
                ;;
            *) exit 2 ;;
        esac
        ;;
    *) cp "${HENOSIS_FIXTURE_RELEASE:?}/${url##*/}" "$output" ;;
esac
EOF
    chmod 755 "$tools/curl"
}

# Create a GitHub CLI double that validates the provenance trust arguments.
make_gh() {
    tools=$1
    cat > "$tools/gh" <<'EOF'
#!/bin/sh
set -eu
[ "$#" -eq 7 ]
[ "$1" = attestation ]
[ "$2" = verify ]
[ -f "$3" ]
[ "$4" = --repo ]
[ "$5" = Syntheos-Systems/henosis ]
[ "$6" = --signer-workflow ]
[ "$7" = Syntheos-Systems/henosis/.github/workflows/ci.yml ]
printf '%s\n' verified > "${HENOSIS_GH_LOG:?}"
exit "${HENOSIS_FIXTURE_GH_EXIT:-0}"
EOF
    chmod 755 "$tools/gh"
}

# Verify a healthy installation selects the archive, verifies it, and initializes it.
test_verified_install() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/release"; case_root="$TEST_ROOT/success"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json"
    grep -F '"ok":true' "$case_root/result.json" >/dev/null || fail 'headless install did not report success'
    [ -x "$case_root/bin/henosis" ] || fail 'henosis was not installed'
    [ -x "$case_root/bin/crucible" ] || fail 'Crucible was not installed'
    [ "$(cat "$case_root/init.log")" = 'init --quick' ] || fail 'installer did not run henosis init --quick'
}

# Verify required provenance pins both the repository and release workflow identities.
test_required_attestation_success() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/attested-release"; case_root="$TEST_ROOT/attested"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; make_gh "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    HENOSIS_SKIP_ATTESTATION=0 HENOSIS_REQUIRE_ATTESTATION=1 \
        HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" \
        HENOSIS_GH_LOG="$case_root/gh.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" \
            --headless > "$case_root/result.json"
    [ "$(cat "$case_root/gh.log")" = verified ] || fail 'attestation verifier was not invoked'
    [ -x "$case_root/bin/henosis" ] || fail 'attested release was not installed'
}

# Verify a failed required provenance check aborts before extraction or installation.
test_required_attestation_failure() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/unattested-release"; case_root="$TEST_ROOT/unattested"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; make_gh "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    if HENOSIS_SKIP_ATTESTATION=0 HENOSIS_REQUIRE_ATTESTATION=1 HENOSIS_FIXTURE_GH_EXIT=1 \
        HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" \
        HENOSIS_GH_LOG="$case_root/gh.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" \
            --headless > "$case_root/result.json" 2>&1; then
        fail 'failed required attestation succeeded'
    fi
    [ "$(cat "$case_root/gh.log")" = verified ] || fail 'failing attestation verifier was not invoked'
    [ ! -e "$case_root/bin/henosis" ] || fail 'failed attestation installed a binary'
    grep -F 'provenance verification failed' "$case_root/result.json" >/dev/null ||
        fail 'failed attestation did not report the trust failure'
}

# Verify a failed initializer restores the precise previous executable.
test_rollback() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/rollback-release"; case_root="$TEST_ROOT/rollback"
    make_release "$version" "$target" 'exit 17' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version" "$case_root/bin"
    cp "$release/$version"/* "$case_root/remote/$version/"
    printf '#!/bin/sh\nprintf previous\n' > "$case_root/bin/henosis"; chmod 755 "$case_root/bin/henosis"
    printf '#!/bin/sh\nprintf previous-crucible\n' > "$case_root/bin/crucible"; chmod 755 "$case_root/bin/crucible"
    if HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then fail 'failed initialization succeeded'; fi
    [ "$("$case_root/bin/henosis")" = previous ] || fail 'previous executable was not restored'
    [ "$("$case_root/bin/crucible")" = previous-crucible ] || fail 'previous Crucible executable was not restored'
    grep -F '"ok":false' "$case_root/result.json" >/dev/null || fail 'headless rollback did not report failure'
}

# Verify a bad mandatory checksum prevents installation.
test_checksum_failure() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/checksum-release"; case_root="$TEST_ROOT/checksum"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    printf '%064d  henosis-%s-%s.tar.gz\n' 0 "${version#v}" "$target" > "$case_root/remote/$version/SHA256SUMS"
    if HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then fail 'bad checksum succeeded'; fi
    [ ! -e "$case_root/bin/henosis" ] || fail 'checksum failure installed a binary'
}

# Verify a mutable release is rejected before its manifest or archive can be installed.
test_mutable_release_failure() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/mutable-release"; case_root="$TEST_ROOT/mutable"
    make_release "$version" "$target" 'exit 0' "$release"
    make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
    cp "$release/$version"/* "$case_root/remote/$version/"
    if HENOSIS_FIXTURE_IMMUTABLE=false HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then fail 'mutable release succeeded'; fi
    [ ! -e "$case_root/bin/henosis" ] || fail 'mutable release installed a binary'
    grep -F 'selected release is not immutable' "$case_root/result.json" >/dev/null || fail 'mutable release did not report the trust failure'
}

# Verify user-controlled release text and nested fields cannot impersonate top-level trust fields.
test_release_metadata_structure() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; release="$TEST_ROOT/metadata-release"
    make_release "$version" "$target" 'exit 0' "$release"
    for mode in embedded-true nested-true duplicate malformed malformed-token malformed-nested; do
        case_root="$TEST_ROOT/metadata-$mode"
        make_curl "$case_root/tools"; mkdir -p "$case_root/remote/$version"
        cp "$release/$version"/* "$case_root/remote/$version/"
        if HENOSIS_FIXTURE_METADATA="$mode" HENOSIS_FIXTURE_RELEASE="$case_root/remote/$version" \
            HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
            "$REPOSITORY_DIR/install.sh" --version "$version" --install-dir "$case_root/bin" \
                --headless > "$case_root/result.json" 2>&1; then
            fail "$mode release metadata succeeded"
        fi
        [ ! -e "$case_root/bin/henosis" ] || fail "$mode release metadata installed a binary"
    done
}

# Verify an extracted release installs its adjacent binary without network access.
test_archive_local_install() {
    version=v0.1.0-alpha.6; target=x86_64-unknown-linux-musl; case_root="$TEST_ROOT/archive-local"
    mkdir -p "$case_root/archive" "$case_root/bin" "$case_root/tools"
    cp "$REPOSITORY_DIR/install.sh" "$case_root/archive/install.sh"
    printf '%s %s\n' "$version" "$target" > "$case_root/archive/HENOSIS_ARCHIVE"
    cat > "$case_root/archive/henosis" <<'EOF'
#!/bin/sh
printf '%s' "$*" > "${HENOSIS_INIT_LOG:?}"
EOF
    printf '#!/bin/sh\nexit 0\n' > "$case_root/archive/crucible"
    cat > "$case_root/tools/curl" <<'EOF'
#!/bin/sh
exit 99
EOF
    chmod 755 "$case_root/archive/install.sh" "$case_root/archive/henosis" "$case_root/archive/crucible" "$case_root/tools/curl"
    HENOSIS_INIT_LOG="$case_root/init.log" PATH="$case_root/tools:$ORIGINAL_PATH" \
        "$case_root/archive/install.sh" --version "$version" --install-dir "$case_root/bin" --headless > "$case_root/result.json"
    [ -x "$case_root/bin/henosis" ] || fail 'archive-local installer did not install the adjacent binary'
    [ -x "$case_root/bin/crucible" ] || fail 'archive-local installer did not install adjacent Crucible'
    [ "$(cat "$case_root/init.log")" = 'init --quick' ] || fail 'archive-local installer did not initialize'
}

# Verify a mismatched archive marker fails closed instead of downloading.
test_archive_marker_mismatch() {
    case_root="$TEST_ROOT/archive-mismatch"
    mkdir -p "$case_root/archive" "$case_root/bin"
    cp "$REPOSITORY_DIR/install.sh" "$case_root/archive/install.sh"
    cp /bin/true "$case_root/archive/henosis"
    cp /bin/true "$case_root/archive/crucible"
    printf '%s\n' 'v9.9.9 x86_64-unknown-linux-musl' > "$case_root/archive/HENOSIS_ARCHIVE"
    chmod 755 "$case_root/archive/install.sh" "$case_root/archive/henosis" "$case_root/archive/crucible"
    if "$case_root/archive/install.sh" --version v0.1.0-alpha.6 --install-dir "$case_root/bin" --headless > "$case_root/result.json" 2>&1; then
        fail 'mismatched archive marker succeeded'
    fi
    [ ! -e "$case_root/bin/henosis" ] || fail 'mismatched archive marker installed a binary'
}

trap cleanup EXIT HUP INT TERM
test_verified_install
test_required_attestation_success
test_required_attestation_failure
test_rollback
test_checksum_failure
test_mutable_release_failure
test_release_metadata_structure
test_archive_local_install
test_archive_marker_mismatch
printf '%s\n' 'installer contract passed'
