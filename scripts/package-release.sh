#!/bin/sh
# Package one Unix Henosis server binary into a reproducible release archive.

set -eu

PROGRAM="package-release"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
REPOSITORY_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd -P)
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-315532800}

# Print an error that identifies the failed release-package precondition.
die() {
    printf '%s: error: %s\n' "$PROGRAM" "$*" >&2
    exit 1
}

# Reject archive components that could escape the controlled staging directory.
validate_component() {
    case "$2" in
        ''|*[!A-Za-z0-9._-]*) die "$1 contains unsupported characters" ;;
    esac
}

# Select a GNU tar implementation so archive ordering and metadata are stable on every Unix runner.
find_gnu_tar() {
    for candidate in gtar tar; do
        if command -v "$candidate" >/dev/null 2>&1 \
            && "$candidate" --version 2>/dev/null | grep -q 'GNU tar'; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    die "GNU tar is required to create reproducible archives"
}

# Write the release-local installation guide that accompanies each native binary.
write_install_readme() {
    readme_path=$1
    version=$2
    target=$3
    printf '# Henosis %s\n\n' "$version" > "$readme_path"
    printf 'This archive contains the native Henosis server binary for `%s`.\n\n' "$target" >> "$readme_path"
    cat >> "$readme_path" <<'EOF'
## Verify and install

Before installation, verify this archive against the `SHA256SUMS` file and
its GitHub artifact attestation on the matching release. The attestation is
signed with the GitHub Actions OIDC identity for Syntheos-Systems/henosis.

For a configured local installation, run the bundled installer:

```sh
./install.sh --binary ./syntheos-server --local
```

The governed mission requires `curl` and Python 3.

The installer creates the local operator identity and restricted runtime
configuration. After the health check passes, prove authorized execution,
hostile-input denial, and correlated audit projection:

```sh
./demo-governed-mission.sh
```

See the repository README and SECURITY.md before exposing a deployment.
EOF
}

[ "$#" -eq 4 ] || die "usage: $0 BINARY VERSION TARGET OUTPUT_DIRECTORY"

binary_path=$1
version=$2
target=$3
output_directory=$4

[ -f "$binary_path" ] || die "binary does not exist: $binary_path"
[ -f "$REPOSITORY_DIR/LICENSE" ] || die "repository LICENSE is missing"
[ -f "$REPOSITORY_DIR/install.sh" ] || die "repository installer is missing"
[ -f "$REPOSITORY_DIR/scripts/demo-governed-mission.sh" ] \
    || die "governed mission script is missing"
case "$SOURCE_DATE_EPOCH" in
    ''|*[!0-9]*) die "SOURCE_DATE_EPOCH must be a non-negative integer" ;;
esac
validate_component "version" "$version"
validate_component "target" "$target"

tar_command=$(find_gnu_tar)
archive_root="henosis-${version}-${target}"
archive_path="$output_directory/${archive_root}.tar.gz"
staging_parent=$(mktemp -d "${TMPDIR:-/tmp}/henosis-release.XXXXXX")
staging_directory="$staging_parent/$archive_root"

# Remove the private staging directory created solely for this archive.
cleanup() {
    rm -rf "$staging_parent"
}

trap cleanup EXIT HUP INT TERM
mkdir -p "$staging_directory" "$output_directory"
install -m 755 "$binary_path" "$staging_directory/syntheos-server"
install -m 644 "$REPOSITORY_DIR/LICENSE" "$staging_directory/LICENSE"
install -m 755 "$REPOSITORY_DIR/install.sh" "$staging_directory/install.sh"
install -m 755 \
    "$REPOSITORY_DIR/scripts/demo-governed-mission.sh" \
    "$staging_directory/demo-governed-mission.sh"
write_install_readme "$staging_directory/README.md" "$version" "$target"
chmod 644 "$staging_directory/README.md"

"$tar_command" \
    --sort=name \
    --mtime="@${SOURCE_DATE_EPOCH}" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$staging_parent" \
    -cf - "$archive_root" \
    | gzip -n > "$archive_path"

printf '%s\n' "$archive_path"
