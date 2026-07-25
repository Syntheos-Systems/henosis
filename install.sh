#!/bin/sh
# Install a verified native Henosis release for the current Unix platform.

set -eu

PROGRAM=henosis-installer
RELEASE_BASE=${HENOSIS_RELEASE_BASE:-https://github.com/Syntheos-Systems/henosis/releases/download}
RELEASE_API=${HENOSIS_RELEASE_API:-https://api.github.com/repos/Syntheos-Systems/henosis/releases/tags}
VERSION=${HENOSIS_VERSION:-v0.1.0-alpha.6}
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
  HENOSIS_VERSION       Release tag, default v0.1.0-alpha.6
  HENOSIS_RELEASE_BASE  Release download base URL
  HENOSIS_RELEASE_API   Release metadata API base URL
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

# Extract the relevant top-level fields from a JSON release document.
release_metadata_fields() {
    awk '
        # Mark the document invalid and return a false parser result.
        function reject() {
            invalid = 1
            return 0
        }

        # Advance past JSON whitespace.
        function skip_whitespace() {
            while (position <= document_length &&
                   substr(document, position, 1) ~ /[[:space:]]/) {
                position++
            }
        }

        # Parse one JSON string while validating every escape sequence.
        function parse_string(    character, escape, hexadecimal) {
            if (substr(document, position, 1) != "\"") {
                return reject()
            }
            position++
            string_value = ""
            string_had_escape = 0
            while (position <= document_length) {
                character = substr(document, position, 1)
                if (character == "\"") {
                    position++
                    return 1
                }
                if (character == "\\") {
                    string_had_escape = 1
                    position++
                    if (position > document_length) {
                        return reject()
                    }
                    escape = substr(document, position, 1)
                    if (escape == "u") {
                        hexadecimal = substr(document, position + 1, 4)
                        if (hexadecimal !~ /^[0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f][0-9A-Fa-f]$/) {
                            return reject()
                        }
                        position += 5
                    } else if (escape == "\"" || escape == "\\" ||
                               escape == "/" || escape == "b" ||
                               escape == "f" || escape == "n" ||
                               escape == "r" || escape == "t") {
                        position++
                    } else {
                        return reject()
                    }
                    string_value = string_value "?"
                    continue
                }
                if (character ~ /[[:cntrl:]]/) {
                    return reject()
                }
                string_value = string_value character
                position++
            }
            return reject()
        }

        # Parse one JSON number and reject leading zeros or incomplete exponents.
        function parse_number(    start, number) {
            start = position
            while (substr(document, position, 1) ~ /[0-9eE+.-]/) {
                position++
            }
            number = substr(document, start, position - start)
            if (number !~ /^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$/) {
                return reject()
            }
            return_kind = "number"
            return_text = number
            return_had_escape = 0
            return 1
        }

        # Parse one bounded JSON array.
        function parse_array(    character) {
            if (depth >= 128) {
                return reject()
            }
            depth++
            position++
            skip_whitespace()
            if (substr(document, position, 1) == "]") {
                position++
                depth--
                return 1
            }
            while (!invalid) {
                if (!parse_value()) {
                    return 0
                }
                skip_whitespace()
                character = substr(document, position, 1)
                if (character == "]") {
                    position++
                    depth--
                    return 1
                }
                if (character != ",") {
                    return reject()
                }
                position++
                skip_whitespace()
            }
            return 0
        }

        # Record one parsed top-level release trust field.
        function record_trust_field(key, kind, value, had_escape) {
            if (key == "tag_name") {
                tag_count++
                if (kind != "string" || had_escape) {
                    return reject()
                }
                tag_name = value
            } else if (key == "draft") {
                draft_count++
                if (kind != "boolean") {
                    return reject()
                }
                draft = value
            } else if (key == "immutable") {
                immutable_count++
                if (kind != "boolean") {
                    return reject()
                }
                immutable = value
            }
            return 1
        }

        # Parse one bounded JSON object and inspect fields on the root object only.
        function parse_object(    character, key, key_had_escape, kind, value, had_escape) {
            if (depth >= 128) {
                return reject()
            }
            depth++
            position++
            skip_whitespace()
            if (substr(document, position, 1) == "}") {
                position++
                depth--
                return 1
            }
            while (!invalid) {
                if (!parse_string()) {
                    return 0
                }
                key = string_value
                key_had_escape = string_had_escape
                if (depth == 1 && key_had_escape) {
                    return reject()
                }
                skip_whitespace()
                if (substr(document, position, 1) != ":") {
                    return reject()
                }
                position++
                if (!parse_value()) {
                    return 0
                }
                kind = return_kind
                value = return_text
                had_escape = return_had_escape
                if (depth == 1 &&
                    !record_trust_field(key, kind, value, had_escape)) {
                    return 0
                }
                skip_whitespace()
                character = substr(document, position, 1)
                if (character == "}") {
                    position++
                    depth--
                    return 1
                }
                if (character != ",") {
                    return reject()
                }
                position++
                skip_whitespace()
            }
            return 0
        }

        # Parse one JSON value and expose its scalar type and value to the caller.
        function parse_value(    character) {
            skip_whitespace()
            character = substr(document, position, 1)
            if (character == "\"") {
                if (!parse_string()) {
                    return 0
                }
                return_kind = "string"
                return_text = string_value
                return_had_escape = string_had_escape
                return 1
            }
            if (character == "{") {
                if (!parse_object()) {
                    return 0
                }
                return_kind = "object"
                return_text = ""
                return_had_escape = 0
                return 1
            }
            if (character == "[") {
                if (!parse_array()) {
                    return 0
                }
                return_kind = "array"
                return_text = ""
                return_had_escape = 0
                return 1
            }
            if (substr(document, position, 4) == "true") {
                position += 4
                return_kind = "boolean"
                return_text = "true"
                return_had_escape = 0
                return 1
            }
            if (substr(document, position, 5) == "false") {
                position += 5
                return_kind = "boolean"
                return_text = "false"
                return_had_escape = 0
                return 1
            }
            if (substr(document, position, 4) == "null") {
                position += 4
                return_kind = "null"
                return_text = "null"
                return_had_escape = 0
                return 1
            }
            return parse_number()
        }

        { document = document $0 "\n" }
        END {
            document_length = length(document)
            position = 1
            depth = 0
            invalid = 0
            tag_count = 0
            draft_count = 0
            immutable_count = 0
            if (!parse_value() || return_kind != "object") {
                invalid = 1
            }
            skip_whitespace()
            if (position <= document_length) {
                invalid = 1
            }
            if (invalid || depth != 0 || tag_count != 1 ||
                draft_count != 1 || immutable_count != 1) {
                exit 1
            }
            print tag_name
            print draft
            print immutable
        }
    ' "$1"
}

# Require the official release metadata to identify a published immutable release.
verify_release_metadata() {
    metadata=$1
    [ "$(wc -c < "$metadata" | tr -d ' ')" -le 1048576 ] ||
        die 'release metadata exceeds the 1 MiB safety limit'
    fields=$(release_metadata_fields "$metadata") ||
        die 'release metadata is not valid JSON with unique trust fields'
    tag_name=$(printf '%s\n' "$fields" | sed -n '1p')
    draft=$(printf '%s\n' "$fields" | sed -n '2p')
    immutable=$(printf '%s\n' "$fields" | sed -n '3p')
    [ "$tag_name" = "$VERSION" ] ||
        die 'release metadata does not match the selected version'
    [ "$draft" = false ] ||
        die 'selected release is not published'
    [ "$immutable" = true ] ||
        die 'selected release is not immutable'
}

# Resolve the adjacent binary from a verified release archive, when present.
archive_binary() {
    target=$1
    case "$0" in
        /*) installer_path=$0 ;;
        */*) installer_path="$(pwd -P)/$0" ;;
        *) return 1 ;;
    esac
    installer_dir=$(CDPATH= cd -- "$(dirname -- "$installer_path")" && pwd -P) ||
        die 'could not resolve the installer directory'
    marker="$installer_dir/HENOSIS_ARCHIVE"
    [ -f "$marker" ] || return 1
    marker_version=
    marker_target=
    marker_extra=
    IFS=' ' read -r marker_version marker_target marker_extra < "$marker" ||
        die 'could not read the release archive marker'
    [ -z "$marker_extra" ] &&
        [ "$marker_version" = "$VERSION" ] &&
        [ "$marker_target" = "$target" ] ||
        die 'release archive marker does not match this installer'
    candidate="$installer_dir/henosis"
    [ -f "$candidate" ] && [ -x "$candidate" ] ||
        die 'release archive does not contain an executable adjacent henosis'
    ARCHIVE_BINARY=$candidate
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

# Activate and initialize one already verified executable.
install_candidate() {
    candidate=$1
    target=$2
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

# Download or resolve, verify, activate, and initialize the native release.
main() {
    parse_args "$@"
    case "$VERSION" in
        v[0-9]*)
            case "$VERSION" in *[!A-Za-z0-9._-]*) die 'release version contains unsupported characters' ;; esac
            ;;
        *) die 'release version must begin with v and contain a numeric version' ;;
    esac
    target=$(release_target)
    WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install.XXXXXX") || die 'could not create temporary directory'
    trap cleanup EXIT HUP INT TERM
    mkdir -p "$INSTALL_DIR"
    [ -d "$INSTALL_DIR" ] || die "install directory is not a directory: $INSTALL_DIR"
    ARCHIVE_BINARY=
    if archive_binary "$target"; then
        candidate=$ARCHIVE_BINARY
        info "installing the verified adjacent $VERSION binary"
    else
        archive_name="henosis-${VERSION#v}-${target}.tar.gz"
        archive="$WORK_DIR/$archive_name"
        manifest="$WORK_DIR/SHA256SUMS"
        metadata="$WORK_DIR/release.json"
        url_base="${RELEASE_BASE%/}/$VERSION"
        metadata_url="${RELEASE_API%/}/$VERSION"
        info "verifying immutable release $VERSION"
        download "$metadata_url" "$metadata" || die 'could not download release metadata'
        verify_release_metadata "$metadata"
        info "downloading $archive_name"
        download "$url_base/SHA256SUMS" "$manifest" || die 'could not download checksum manifest'
        download "$url_base/$archive_name" "$archive" || die "could not download $archive_name"
        verify_archive "$manifest" "$archive" "$archive_name"
        tar -xzf "$archive" -C "$WORK_DIR" || die 'could not extract verified archive'
        candidate="$WORK_DIR/henosis-${VERSION#v}-${target}/henosis"
        [ -f "$candidate" ] && [ -x "$candidate" ] || die 'verified archive does not contain an executable henosis'
    fi
    install_candidate "$candidate" "$target"
}

main "$@"
