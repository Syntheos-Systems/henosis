#!/bin/sh
# Install a verified native Henosis release for the current Unix platform.

set -eu

PROGRAM=henosis-installer
RELEASE_BASE=${HENOSIS_RELEASE_BASE:-https://github.com/Syntheos-Systems/henosis/releases/download}
VERSION=${HENOSIS_VERSION:-v0.1.0-alpha.1}
INSTALL_DIR=${HENOSIS_INSTALL_DIR:-"${HOME}/.local/bin"}
HEADLESS=0

# Print an installer message to standard error.
info() {
    printf '%s: %s\n' "$PROGRAM" "$*" >&2
}

# Stop the installer, using JSON in explicitly headless mode.
die() {
    if [ "$HEADLESS" -eq 1 ]; then
        printf '{"ok":false,"error":"%s"}\n' "$(printf '%s' "$*" | tr '"' "'")"
    else
        printf '%s: error: %s\n' "$PROGRAM" "$*" >&2
    fi
    exit 1
}

# Display the supported installer interface.
usage() {
    cat <<'EOF'
Usage: install.sh [--version TAG] [--install-dir DIRECTORY] [--headless]

Downloads the native Henosis release for this operating system and CPU,
verifies its mandatory SHA-256 checksum, installs it per-user, and runs:
  henosis init --quick

Environment:
  HENOSIS_VERSION       Release tag, default v0.1.0-alpha.1
  HENOSIS_RELEASE_BASE  Release download base URL
  HENOSIS_INSTALL_DIR   Destination directory, default ~/.local/bin
EOF
}

# Parse command-line options into installer state.
parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version) [ "$#" -ge 2 ] || die '--version requires a value'; VERSION=$2; shift 2 ;;
            --install-dir) [ "$#" -ge 2 ] || die '--install-dir requires a value'; INSTALL_DIR=$2; shift 2 ;;
            --headless) HEADLESS=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done
}

# Map the current Unix platform to the release target triple.
release_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-musl ;;
        Linux:aarch64|Linux:arm64) printf '%s\n' aarch64-unknown-linux-musl ;;
        Darwin:x86_64) printf '%s\n' x86_64-apple-darwin ;;
        Darwin:arm64) printf '%s\n' aarch64-apple-darwin ;;
        *) die "unsupported platform: $os/$arch" ;;
    esac
}

# Download a URL into an explicit path using an available secure downloader.
download() {
    url=$1
    destination=$2
    if command -v curl >/dev/null 2>&1; then
        curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 --output "$destination" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only -qO "$destination" "$url"
    else
        die 'curl or wget is required to download Henosis'
    fi
}

# Calculate a SHA-256 digest in the standard lowercase hexadecimal representation.
sha256() {
    file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    else
        die 'sha256sum or shasum is required to verify Henosis'
    fi
}

# Verify an archive against the release manifest and fail closed on every mismatch.
verify_archive() {
    manifest=$1
    archive=$2
    name=$3
    expected=$(awk -v name="$name" '$2 == name || $2 == "*" name { print $1 }' "$manifest")
    [ "$(printf '%s\n' "$expected" | wc -l | tr -d ' ')" -eq 1 ] || die "checksum manifest has no unique entry for $name"
    case "$expected" in *[!0-9a-fA-F]*|'') die "checksum manifest contains an invalid checksum for $name" ;; esac
    [ "${#expected}" -eq 64 ] || die "checksum manifest contains an invalid checksum for $name"
    actual=$(sha256 "$archive")
    [ "$actual" = "$expected" ] || die "checksum verification failed for $name"
}

# Restore the previous installation after a failed transactional activation.
rollback() {
    if [ "${ACTIVATED:-0}" -eq 1 ]; then
        if [ -n "${BACKUP:-}" ] && [ -f "$BACKUP" ]; then
            mv -f "$BACKUP" "$DESTINATION"
        else
            rm -f "$DESTINATION"
        fi
    fi
}

# Remove only the private temporary workspace created by this installer.
cleanup() {
    rollback
    [ -n "${WORK_DIR:-}" ] && rm -rf "$WORK_DIR"
}

# Download, verify, activate, and initialize the native release.
main() {
    parse_args "$@"
    target=$(release_target)
    archive_name="henosis-${VERSION#v}-${target}.tar.gz"
    WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install.XXXXXX") || die 'could not create temporary directory'
    trap cleanup EXIT HUP INT TERM
    mkdir -p "$INSTALL_DIR"
    [ -d "$INSTALL_DIR" ] || die "install directory is not a directory: $INSTALL_DIR"
    archive="$WORK_DIR/$archive_name"
    manifest="$WORK_DIR/SHA256SUMS"
    url_base="${RELEASE_BASE%/}/$VERSION"
    info "downloading $archive_name"
    download "$url_base/SHA256SUMS" "$manifest" || die 'could not download checksum manifest'
    download "$url_base/$archive_name" "$archive" || die "could not download $archive_name"
    verify_archive "$manifest" "$archive" "$archive_name"
    tar -xzf "$archive" -C "$WORK_DIR" || die 'could not extract verified archive'
    candidate="$WORK_DIR/henosis-${VERSION#v}-${target}/henosis"
    [ -f "$candidate" ] && [ -x "$candidate" ] || die 'verified archive does not contain an executable henosis'
    DESTINATION="$INSTALL_DIR/henosis"
    BACKUP="$WORK_DIR/henosis.previous"
    ACTIVATED=0
    if [ -f "$DESTINATION" ]; then cp -p "$DESTINATION" "$BACKUP"; fi
    install -m 755 "$candidate" "$WORK_DIR/henosis.new"
    mv -f "$WORK_DIR/henosis.new" "$DESTINATION"
    ACTIVATED=1
    if ! "$DESTINATION" init --quick; then
        die 'henosis init --quick failed; restored the previous installation'
    fi
    ACTIVATED=0
    rm -f "$BACKUP"
    if [ "$HEADLESS" -eq 1 ]; then
        printf '{"ok":true,"binary":"%s","version":"%s","target":"%s"}\n' "$DESTINATION" "$VERSION" "$target"
    else
        info "installed $DESTINATION"
    fi
}

main "$@"
