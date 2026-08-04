#!/bin/sh
# Verify desktop version parity and the unified release artifact contract.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_DIRECTORY=$(mktemp -d "${TMPDIR:-/tmp}/henosis-desktop-release-test.XXXXXX")
RELEASE_DIRECTORY="$TEST_DIRECTORY/release"
DESKTOP_INSTALL_GUIDE="$REPOSITORY_DIR/docs/desktop-install.md"
VERSION=0.1.0-alpha.6

# Remove only the isolated desktop release test workspace.
cleanup() { rm -rf "$TEST_DIRECTORY"; }

# Stop the desktop release contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Create the exact artifact fixture accepted by the publication validator.
create_release_fixture() {
    for filename in \
        "henosis-$VERSION-aarch64-apple-darwin.tar.gz" \
        "henosis-$VERSION-aarch64-unknown-linux-musl.tar.gz" \
        "henosis-$VERSION-x86_64-apple-darwin.tar.gz" \
        "henosis-$VERSION-x86_64-pc-windows-msvc.zip" \
        "henosis-$VERSION-x86_64-unknown-linux-musl.tar.gz" \
        "henosis-desktop-$VERSION-linux-x86_64.AppImage" \
        "henosis-desktop-$VERSION-linux-x86_64.deb" \
        "henosis-desktop-$VERSION-macos-aarch64.dmg" \
        "henosis-desktop-$VERSION-macos-x86_64.dmg" \
        "henosis-desktop-$VERSION-windows-x86_64.exe"
    do
        : > "$RELEASE_DIRECTORY/$filename"
    done
}

trap cleanup EXIT HUP INT TERM
mkdir "$RELEASE_DIRECTORY"

server_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPOSITORY_DIR/crates/syntheos-server/Cargo.toml")
desktop_package_version=$(sed -n 's/^[[:space:]]*"version": "\([^"]*\)",*$/\1/p' "$REPOSITORY_DIR/apps/desktop/package.json")
desktop_cargo_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPOSITORY_DIR/apps/desktop/src-tauri/Cargo.toml")
desktop_config_version=$(sed -n 's/^[[:space:]]*"version": "\([^"]*\)",*$/\1/p' "$REPOSITORY_DIR/apps/desktop/src-tauri/tauri.conf.json")

[ "$server_version" = "$VERSION" ] || fail 'server version differs from release contract'
[ "$desktop_package_version" = "$VERSION" ] || fail 'desktop package version differs from server'
[ "$desktop_cargo_version" = "$VERSION" ] || fail 'desktop Cargo version differs from server'
[ "$desktop_config_version" = "$VERSION" ] || fail 'desktop Tauri version differs from server'
grep -Fx '[workspace]' "$REPOSITORY_DIR/apps/desktop/src-tauri/Cargo.toml" >/dev/null ||
    fail 'desktop crate is not isolated from the server Cargo workspace'

for installer_name in \
    'henosis-desktop-{version}-linux-x86_64.AppImage' \
    'henosis-desktop-{version}-linux-x86_64.deb' \
    'henosis-desktop-{version}-macos-aarch64.dmg' \
    'henosis-desktop-{version}-macos-x86_64.dmg' \
    'henosis-desktop-{version}-windows-x86_64.exe'
do
    grep -F "$installer_name" "$DESKTOP_INSTALL_GUIDE" >/dev/null ||
        fail "desktop install guide omits $installer_name"
done
grep -F '`v0.1.0-alpha.6` contains headless archives' "$DESKTOP_INSTALL_GUIDE" >/dev/null ||
    fail 'desktop install guide misstates the current release'
grep -F 'does not start or install Rift' "$DESKTOP_INSTALL_GUIDE" >/dev/null ||
    fail 'desktop install guide implies local Rift provisioning'
grep -F 'not Apple-notarized' "$DESKTOP_INSTALL_GUIDE" >/dev/null ||
    fail 'desktop install guide omits the macOS trust boundary'
grep -F 'not Authenticode-signed' "$DESKTOP_INSTALL_GUIDE" >/dev/null ||
    fail 'desktop install guide omits the Windows trust boundary'

create_release_fixture
"$REPOSITORY_DIR/scripts/validate-release-assets.sh" "$RELEASE_DIRECTORY" "$VERSION"
mv "$RELEASE_DIRECTORY/henosis-desktop-$VERSION-windows-x86_64.exe" \
    "$TEST_DIRECTORY/henosis-desktop-$VERSION-windows-x86_64.exe.missing"
if "$REPOSITORY_DIR/scripts/validate-release-assets.sh" "$RELEASE_DIRECTORY" "$VERSION" >/dev/null 2>&1; then
    fail 'release validator accepted a missing desktop installer'
fi

printf '%s\n' 'desktop release contract passed'
