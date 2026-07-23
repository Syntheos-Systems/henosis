#!/bin/sh
# Validate reproducible Unix release packaging without invoking Cargo.

set -eu

PROGRAM="release-package-test"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
TEST_DIRECTORY=$(mktemp -d "${TMPDIR:-/tmp}/henosis-release-test.XXXXXX")

# Remove the isolated package-test workspace on every exit path.
cleanup() {
    rm -rf "$TEST_DIRECTORY"
}

# Stop the test with an assertion message that is visible in CI logs.
fail() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

trap cleanup EXIT HUP INT TERM
binary_path="$TEST_DIRECTORY/syntheos-server"
first_output="$TEST_DIRECTORY/first"
second_output="$TEST_DIRECTORY/second"
archive_name="henosis-0.1.0-x86_64-unknown-linux-musl.tar.gz"

printf '#!/bin/sh\nprintf "henosis test binary\\n"\n' > "$binary_path"
chmod 755 "$binary_path"

SOURCE_DATE_EPOCH=1784768092 \
    "$REPOSITORY_DIR/scripts/package-release.sh" \
    "$binary_path" \
    "0.1.0" \
    "x86_64-unknown-linux-musl" \
    "$first_output" >/dev/null
SOURCE_DATE_EPOCH=1784768092 \
    "$REPOSITORY_DIR/scripts/package-release.sh" \
    "$binary_path" \
    "0.1.0" \
    "x86_64-unknown-linux-musl" \
    "$second_output" >/dev/null

first_archive="$first_output/$archive_name"
second_archive="$second_output/$archive_name"
[ -f "$first_archive" ] || fail "first package archive was not created"
[ -f "$second_archive" ] || fail "second package archive was not created"
cmp "$first_archive" "$second_archive" || fail "release archive is not reproducible"

members=$(tar -tzf "$first_archive")
expected_members='henosis-0.1.0-x86_64-unknown-linux-musl/
henosis-0.1.0-x86_64-unknown-linux-musl/LICENSE
henosis-0.1.0-x86_64-unknown-linux-musl/README.md
henosis-0.1.0-x86_64-unknown-linux-musl/demo-governed-mission.sh
henosis-0.1.0-x86_64-unknown-linux-musl/install.sh
henosis-0.1.0-x86_64-unknown-linux-musl/syntheos-server'
[ "$members" = "$expected_members" ] || fail "archive members differ from the release contract"

tar -xOf "$first_archive" henosis-0.1.0-x86_64-unknown-linux-musl/README.md \
    | grep -F 'GitHub artifact attestation' >/dev/null \
    || fail "release README does not document artifact verification"

binary_mode=$(tar -tvzf "$first_archive" \
    | awk '$NF ~ /\/syntheos-server$/ { print substr($1, 1, 10) }')
[ "$binary_mode" = "-rwxr-xr-x" ] || fail "packaged Unix binary is not mode 0755"
mission_mode=$(tar -tvzf "$first_archive" \
    | awk '$NF ~ /\/demo-governed-mission.sh$/ { print substr($1, 1, 10) }')
[ "$mission_mode" = "-rwxr-xr-x" ] || fail "packaged mission is not mode 0755"

(CDPATH= cd -- "$first_output" && sha256sum "$archive_name" > SHA256SUMS)
grep -Eq "^[0-9a-f]{64}  ${archive_name}$" "$first_output/SHA256SUMS" \
    || fail "checksum file does not use the release contract format"
(CDPATH= cd -- "$first_output" && sha256sum --check SHA256SUMS >/dev/null) \
    || fail "release checksum did not verify"

workflow="$REPOSITORY_DIR/.github/workflows/ci.yml"
for target in \
    x86_64-unknown-linux-musl \
    x86_64-apple-darwin \
    aarch64-apple-darwin \
    x86_64-pc-windows-msvc; do
    grep -F "target: $target" "$workflow" >/dev/null \
        || fail "release workflow is missing target $target"
done
grep -F 'artifact-metadata: write' "$workflow" >/dev/null \
    || fail "release attestation permission is missing"
grep -F 'actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6 # v4.2.0' "$workflow" >/dev/null \
    || fail "release attestation action is not pinned to the reviewed revision"
grep -F 'syntheos-server.exe' "$REPOSITORY_DIR/scripts/package-release.ps1" >/dev/null \
    || fail "Windows package contract does not contain syntheos-server.exe"

if SOURCE_DATE_EPOCH=1784768092 \
    "$REPOSITORY_DIR/scripts/package-release.sh" \
    "$TEST_DIRECTORY/missing" \
    "0.1.0" \
    "x86_64-unknown-linux-musl" \
    "$TEST_DIRECTORY/invalid" >/dev/null 2>&1; then
    fail "packager accepted a missing binary"
fi

printf '%s\n' "release package contract passed"
