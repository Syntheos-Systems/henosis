#!/bin/sh
# Package one Unix Henosis executable into a reproducible native archive.

set -eu

PROGRAM=package-release
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-315532800}

# Stop packaging with a release-specific validation error.
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

# Reject archive components that could escape the controlled staging directory.
validate_component() { case "$2" in ''|*[!A-Za-z0-9._-]*) die "$1 contains unsupported characters" ;; esac; }

# Select GNU tar so archive ordering and metadata are stable on every Unix runner.
find_gnu_tar() {
    for candidate in gtar tar; do
        if command -v "$candidate" >/dev/null 2>&1 && "$candidate" --version 2>/dev/null | grep -q 'GNU tar'; then printf '%s\n' "$candidate"; return; fi
    done
    die 'GNU tar is required to create reproducible archives'
}

# Write the release-local installation instructions.
write_install_readme() {
    cat > "$1" <<EOF
# Henosis $2

This archive contains the native \`henosis\` executable for \`$3\`.

Verify this archive with the release \`SHA256SUMS\` manifest before installing.
Run \`./install.sh --headless\` from the extracted archive for a per-user verified installation.
The installer runs \`henosis init --quick\` and rolls back if initialization fails.
EOF
}

[ "$#" -eq 4 ] || die "usage: $0 BINARY VERSION TARGET OUTPUT_DIRECTORY"
binary_path=$1; version=$2; target=$3; output_directory=$4
[ -f "$binary_path" ] && [ -x "$binary_path" ] || die "binary is missing or not executable: $binary_path"
[ -f "$REPOSITORY_DIR/LICENSE" ] || die 'repository LICENSE is missing'
[ -f "$REPOSITORY_DIR/install.sh" ] || die 'repository Unix installer is missing'
case "$SOURCE_DATE_EPOCH" in ''|*[!0-9]*) die 'SOURCE_DATE_EPOCH must be a non-negative integer' ;; esac
validate_component version "$version"; validate_component target "$target"
tar_command=$(find_gnu_tar)
root="henosis-${version}-${target}"
archive="$output_directory/${root}.tar.gz"
stage_parent=$(mktemp -d "${TMPDIR:-/tmp}/henosis-release.XXXXXX") || die 'could not create staging directory'

# Remove only the private package staging directory.
cleanup() { rm -rf "$stage_parent"; }
trap cleanup EXIT HUP INT TERM
stage="$stage_parent/$root"
mkdir -p "$stage" "$output_directory"
install -m 755 "$binary_path" "$stage/henosis"
install -m 755 "$REPOSITORY_DIR/install.sh" "$stage/install.sh"
install -m 644 "$REPOSITORY_DIR/LICENSE" "$stage/LICENSE"
write_install_readme "$stage/README.md" "$version" "$target"
"$tar_command" --sort=name --mtime="@${SOURCE_DATE_EPOCH}" --owner=0 --group=0 --numeric-owner -C "$stage_parent" -cf - "$root" | gzip -n > "$archive"
printf '%s\n' "$archive"
