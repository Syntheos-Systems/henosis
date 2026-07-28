#!/bin/sh
# Validate the complete native and desktop artifact set before publication.

set -eu

PROGRAM=validate-release-assets

# Stop publication with a precise artifact-contract diagnostic.
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

# Print the exact public release filenames for one Henosis version.
expected_release_files() {
    version=$1
    printf '%s\n' \
        "henosis-$version-aarch64-apple-darwin.tar.gz" \
        "henosis-$version-aarch64-unknown-linux-musl.tar.gz" \
        "henosis-$version-x86_64-apple-darwin.tar.gz" \
        "henosis-$version-x86_64-pc-windows-msvc.zip" \
        "henosis-$version-x86_64-unknown-linux-musl.tar.gz" \
        "henosis-desktop-$version-linux-x86_64.AppImage" \
        "henosis-desktop-$version-linux-x86_64.deb" \
        "henosis-desktop-$version-macos-aarch64.dmg" \
        "henosis-desktop-$version-macos-x86_64.dmg" \
        "henosis-desktop-$version-windows-x86_64.exe"
}

# Validate one artifact directory and version without following symlinks.
main() {
    [ "$#" -eq 2 ] || die "usage: $0 RELEASE_DIRECTORY VERSION"
    release_directory=$1
    version=$2
    [ -d "$release_directory" ] || die "release directory does not exist: $release_directory"
    case "$version" in
        '' | *[!0-9A-Za-z.-]*) die "release version is invalid: $version" ;;
    esac

    expected=$(expected_release_files "$version" | LC_ALL=C sort)
    actual=$(find "$release_directory" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)
    if [ "$actual" != "$expected" ]; then
        printf '%s\n%s\n' 'Expected release files:' "$expected" >&2
        printf '%s\n%s\n' 'Actual release files:' "$actual" >&2
        die 'release artifacts differ from the complete contract'
    fi
    expected_release_files "$version" | while IFS= read -r filename; do
        [ -f "$release_directory/$filename" ] ||
            die "release artifact is not a regular file: $filename"
        [ ! -L "$release_directory/$filename" ] ||
            die "release artifact must not be a symbolic link: $filename"
    done
}

main "$@"
