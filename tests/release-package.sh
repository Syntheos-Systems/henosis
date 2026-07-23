#!/bin/sh
# Verify the Unix native archive contains only the public Henosis launch contract.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_DIRECTORY=$(mktemp -d "${TMPDIR:-/tmp}/henosis-package-test.XXXXXX")

# Remove only the package test workspace.
cleanup() { rm -rf "$TEST_DIRECTORY"; }

# Stop the release contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

trap cleanup EXIT HUP INT TERM
binary="$TEST_DIRECTORY/henosis"; printf '#!/bin/sh\nexit 0\n' > "$binary"; chmod 755 "$binary"
SOURCE_DATE_EPOCH=1784768092 "$REPOSITORY_DIR/scripts/package-release.sh" "$binary" 0.1.0-alpha.1 x86_64-unknown-linux-musl "$TEST_DIRECTORY/dist" >/dev/null
archive="$TEST_DIRECTORY/dist/henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl.tar.gz"
[ -f "$archive" ] || fail 'archive was not created'
members=$(tar -tzf "$archive")
expected='henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/LICENSE
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/README.md
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/henosis
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/install.sh'
[ "$members" = "$expected" ] || fail 'archive members differ from contract'
tar -xOf "$archive" henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/README.md | grep -F 'henosis init --quick' >/dev/null || fail 'archive does not document initialization'
grep -F 'aarch64-unknown-linux-musl' "$REPOSITORY_DIR/.github/workflows/ci.yml" >/dev/null || fail 'workflow lacks Linux arm64'
grep -F 'henosis init --quick' "$REPOSITORY_DIR/install.sh" >/dev/null || fail 'installer lacks initialization contract'
printf '%s\n' 'release package contract passed'
