#!/bin/sh
# Install a verified native Henosis release for the current Unix platform.

set -eu

PROGRAM=henosis-installer
DEFAULT_RELEASE_BASE=https://github.com/Syntheos-Systems/henosis/releases/download
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

# Resolve one rename-boundary override: SYNTHEOS_<NAME> is canonical and
# HENOSIS_<NAME> is a read-only compatibility alias. Both set with different
# values fails closed. The NAME argument is always a literal chosen below and
# the second argument is the value used only when neither form is present.
resolve_override() {
    if canonical_value=$(printenv "SYNTHEOS_$1"); then canonical_is_set=1; else canonical_is_set=; fi
    if legacy_value=$(printenv "HENOSIS_$1"); then legacy_is_set=1; else legacy_is_set=; fi
    if [ -n "$canonical_is_set" ] && [ -n "$legacy_is_set" ] &&
        [ "$canonical_value" != "$legacy_value" ]; then
        die "SYNTHEOS_$1 and HENOSIS_$1 are both set with different values; unset one (SYNTHEOS_$1 is canonical)"
    fi
    if [ -n "$canonical_is_set" ]; then
        RESOLVED_OVERRIDE=$canonical_value
    elif [ -n "$legacy_is_set" ]; then
        RESOLVED_OVERRIDE=$legacy_value
    else
        RESOLVED_OVERRIDE=$2
    fi
}

resolve_override RELEASE_BASE "$DEFAULT_RELEASE_BASE"
RELEASE_BASE=$RESOLVED_OVERRIDE
resolve_override RELEASE_API https://api.github.com/repos/Syntheos-Systems/henosis/releases/tags
RELEASE_API=$RESOLVED_OVERRIDE
resolve_override VERSION v0.1.0-beta.1
VERSION=$RESOLVED_OVERRIDE
resolve_override INSTALL_DIR "${HOME}/.local/bin"
INSTALL_DIR=$RESOLVED_OVERRIDE
resolve_override EXPERIENCE cli
EXPERIENCE=$RESOLVED_OVERRIDE
resolve_override ATTESTATION_REPO Syntheos-Systems/henosis
ATTESTATION_REPO=$RESOLVED_OVERRIDE
resolve_override SKIP_ATTESTATION 0
SKIP_ATTESTATION=$RESOLVED_OVERRIDE
resolve_override REQUIRE_ATTESTATION 0
REQUIRE_ATTESTATION=$RESOLVED_OVERRIDE
ATTESTATION_SIGNER_WORKFLOW=Syntheos-Systems/henosis/.github/workflows/ci.yml

# Display the supported installer interface.
usage() {
    cat <<'EOF'
Usage: install.sh [--version TAG] [--install-dir DIRECTORY]
                  [--experience cli|chat|desktop|spatial] [--headless]

Downloads and verifies the selected native Henosis experience. CLI installs
Henosis and Crucible per-user and runs `henosis init --quick`. Chat, desktop,
and spatial install the same signed desktop application with a different
initial renderer preference.

Environment (SYNTHEOS_* canonical; legacy HENOSIS_* aliases honored read-only):
  SYNTHEOS_VERSION       Release tag, default v0.1.0-beta.1
  SYNTHEOS_RELEASE_BASE  Release download base URL
  SYNTHEOS_RELEASE_API   Release metadata API base URL
  SYNTHEOS_INSTALL_DIR   Destination directory, default ~/.local/bin
  SYNTHEOS_EXPERIENCE    Initial experience: cli, chat, desktop, or spatial
  SYNTHEOS_REQUIRE_ATTESTATION
                        Set to 1 to require sigstore build-provenance verification
                        via the GitHub CLI and refuse to install without it.
                        Default is opportunistic: verified when gh is present.
  SYNTHEOS_HARNESS       Agent harness for the generated roster, default synapse.
                        Presets: synapse, claude-code, aider. Any other binary
                        must implement distinct --henosis-discuss and
                        --henosis-execute adapter modes.

Examples:
  ./install.sh --experience desktop
  ./install.sh --experience spatial --headless
  curl -fsSL <url>/install.sh | SYNTHEOS_EXPERIENCE=cli sh
  henosis init --quick --harness /opt/bin/my-agent
EOF
}

# Parse command-line options into installer state.
parse_args() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version) [ "$#" -ge 2 ] || die '--version requires a value'; VERSION=$2; shift 2 ;;
            --install-dir) [ "$#" -ge 2 ] || die '--install-dir requires a value'; INSTALL_DIR=$2; shift 2 ;;
            --experience) [ "$#" -ge 2 ] || die '--experience requires a value'; EXPERIENCE=$2; shift 2 ;;
            --headless) HEADLESS=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) die "unknown option: $1" ;;
        esac
    done
}

# Reject unknown experience values before downloads or filesystem mutation.
validate_experience() {
    case "$EXPERIENCE" in
        cli|chat|desktop|spatial) ;;
        *) die "unknown experience: $EXPERIENCE (choose cli, chat, desktop, or spatial)" ;;
    esac
}

# Test whether the selected experience uses the shared desktop application.
is_desktop_experience() {
    [ "$EXPERIENCE" != cli ]
}

# Map this Unix host to one stable public desktop release asset.
desktop_release_asset() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os:$arch" in
        Linux:x86_64|Linux:amd64)
            DESKTOP_ASSET="henosis-desktop-${VERSION#v}-linux-x86_64.AppImage"
            DESKTOP_KIND=appimage
            ;;
        Darwin:x86_64)
            DESKTOP_ASSET="henosis-desktop-${VERSION#v}-macos-x86_64.dmg"
            DESKTOP_KIND=dmg
            ;;
        Darwin:arm64|Darwin:aarch64)
            DESKTOP_ASSET="henosis-desktop-${VERSION#v}-macos-aarch64.dmg"
            DESKTOP_KIND=dmg
            ;;
        *) die "the $EXPERIENCE desktop experience is not packaged for $os/$arch" ;;
    esac
}

# Resolve the Tauri application-data directory used by the current Unix host.
desktop_preference_dir() {
    case "$(uname -s)" in
        Darwin) printf '%s\n' "${HOME}/Library/Application Support/systems.syntheos.henosis" ;;
        *) printf '%s\n' "${XDG_DATA_HOME:-${HOME}/.local/share}/systems.syntheos.henosis" ;;
    esac
}

# Atomically persist the selected non-secret graphical renderer preference.
write_desktop_preference() {
    preference_dir=$(desktop_preference_dir)
    mkdir -p "$preference_dir" || die 'could not create the Henosis application-data directory'
    preference_new="$preference_dir/.experience.json.new.$$"
    printf '"%s"\n' "$EXPERIENCE" > "$preference_new" ||
        die 'could not write the Henosis experience preference'
    chmod 600 "$preference_new" || die 'could not protect the Henosis experience preference'
    mv -f "$preference_new" "$preference_dir/experience.json" ||
        die 'could not activate the Henosis experience preference'
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

# Verify the archive's build provenance against its sigstore attestation.
#
# The checksum manifest is served from the same release as the archive, so a
# matching checksum only proves the file is the one that was uploaded there --
# not that Henosis's release workflow built it. The release job publishes
# sigstore provenance for exactly this purpose; `gh attestation verify` checks
# that chain against the signing identity, which same-origin checksums cannot.
#
# Verification runs when the GitHub CLI is available. Set
# SYNTHEOS_REQUIRE_ATTESTATION=1 to refuse to install without it.
verify_attestation() {
    archive=$1
    name=$2
    # SYNTHEOS_SKIP_ATTESTATION exists for the contract test, which serves a
    # synthetic release through stubbed download tooling; that fixture has no
    # attestation and must not reach the network to discover it.
    #
    # A non-default release base is a mirror or a fork, which likewise does not
    # carry this repo's attestations.
    if [ "$SKIP_ATTESTATION" = 1 ] || [ "$RELEASE_BASE" != "$DEFAULT_RELEASE_BASE" ]; then
        if [ "$REQUIRE_ATTESTATION" = 1 ]; then
            die 'SYNTHEOS_REQUIRE_ATTESTATION=1 requires the default release base and no skip override'
        fi
        info "skipping provenance verification for $name (non-canonical release source)"
        return 0
    fi
    if ! command -v gh >/dev/null 2>&1; then
        if [ "$REQUIRE_ATTESTATION" = 1 ]; then
            die 'SYNTHEOS_REQUIRE_ATTESTATION=1 but the GitHub CLI (gh) is not installed'
        fi
        info "skipping provenance verification for $name (gh not installed; set SYNTHEOS_REQUIRE_ATTESTATION=1 to require it)"
        return 0
    fi
    info "verifying build provenance for $name"
    if gh attestation verify "$archive" --repo "$ATTESTATION_REPO" \
        --signer-workflow "$ATTESTATION_SIGNER_WORKFLOW" >/dev/null 2>&1; then
        info "provenance verified for $name"
        return 0
    fi
    if [ "$REQUIRE_ATTESTATION" = 1 ]; then
        die "provenance verification failed for $name"
    fi
    info "WARNING: provenance verification failed for $name; continuing on checksum alone"
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

# Resolve the adjacent Henosis and Crucible binaries from a verified release archive.
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
    crucible_candidate="$installer_dir/crucible"
    [ -f "$candidate" ] && [ -x "$candidate" ] ||
        die 'release archive does not contain an executable adjacent henosis'
    [ -f "$crucible_candidate" ] && [ -x "$crucible_candidate" ] ||
        die 'release archive does not contain an executable adjacent crucible'
    ARCHIVE_BINARY=$candidate
    ARCHIVE_CRUCIBLE=$crucible_candidate
}

# Restore one backup through a same-directory candidate so cross-device temp
# directories cannot break rollback.
restore_backup() {
    backup=$1
    destination=$2
    restore_candidate="${destination}.restore.$$"
    cp -p "$backup" "$restore_candidate" || return 1
    mv -f "$restore_candidate" "$destination" || {
        rm -f "$restore_candidate"
        return 1
    }
}

# Restore all previous installer-owned files after a failed transactional activation.
rollback() {
    ROLLBACK_FAILED=0
    if [ "${HENOSIS_ACTIVATED:-0}" -eq 1 ]; then
        if [ -n "${HENOSIS_BACKUP:-}" ] && [ -f "$HENOSIS_BACKUP" ]; then
            if ! restore_backup "$HENOSIS_BACKUP" "$HENOSIS_DESTINATION"; then
                info "CRITICAL: could not restore $HENOSIS_DESTINATION"
                ROLLBACK_FAILED=1
            fi
        else
            if ! rm -f "$HENOSIS_DESTINATION"; then ROLLBACK_FAILED=1; fi
        fi
    fi
    if [ "${CRUCIBLE_ACTIVATED:-0}" -eq 1 ]; then
        if [ -n "${CRUCIBLE_BACKUP:-}" ] && [ -f "$CRUCIBLE_BACKUP" ]; then
            if ! restore_backup "$CRUCIBLE_BACKUP" "$CRUCIBLE_DESTINATION"; then
                info "CRITICAL: could not restore $CRUCIBLE_DESTINATION"
                ROLLBACK_FAILED=1
            fi
        else
            if ! rm -f "$CRUCIBLE_DESTINATION"; then ROLLBACK_FAILED=1; fi
        fi
    fi
    if [ "${MANIFEST_ACTIVATED:-0}" -eq 1 ]; then
        if [ -n "${MANIFEST_BACKUP:-}" ] && [ -f "$MANIFEST_BACKUP" ]; then
            if ! restore_backup "$MANIFEST_BACKUP" "$MANIFEST_DESTINATION"; then
                info "CRITICAL: could not restore $MANIFEST_DESTINATION"
                ROLLBACK_FAILED=1
            fi
        else
            if ! rm -f "$MANIFEST_DESTINATION"; then ROLLBACK_FAILED=1; fi
        fi
    fi
    if [ "${DESKTOP_ACTIVATED:-0}" -eq 1 ]; then
        if [ -n "${DESKTOP_BACKUP:-}" ] && [ -f "$DESKTOP_BACKUP" ]; then
            if ! restore_backup "$DESKTOP_BACKUP" "$DESKTOP_DESTINATION"; then
                info "CRITICAL: could not restore $DESKTOP_DESTINATION"
                ROLLBACK_FAILED=1
            fi
        else
            if ! rm -f "$DESKTOP_DESTINATION"; then ROLLBACK_FAILED=1; fi
        fi
    fi
}

# Activate one verified desktop asset and persist its initial renderer.
install_desktop_candidate() {
    candidate=$1
    asset_name=$2
    if [ "$DESKTOP_KIND" = appimage ]; then
        DESKTOP_DESTINATION="$INSTALL_DIR/henosis-desktop"
        DESKTOP_BACKUP="$WORK_DIR/henosis-desktop.previous"
        DESKTOP_STAGE="$INSTALL_DIR/.henosis-desktop.new.$$"
        DESKTOP_ACTIVATED=0
        mkdir -p "$INSTALL_DIR"
        [ -d "$INSTALL_DIR" ] || die "install directory is not a directory: $INSTALL_DIR"
        if [ -f "$DESKTOP_DESTINATION" ]; then cp -p "$DESKTOP_DESTINATION" "$DESKTOP_BACKUP"; fi
        install -m 755 "$candidate" "$DESKTOP_STAGE"
        mv -f "$DESKTOP_STAGE" "$DESKTOP_DESTINATION"
        DESKTOP_ACTIVATED=1
        write_desktop_preference
        DESKTOP_ACTIVATED=0
        rm -f "$DESKTOP_BACKUP"
        if [ "$HEADLESS" -eq 1 ]; then
            printf '{"ok":true,"experience":"%s","desktop":"%s","version":"%s"}\n' \
                "$EXPERIENCE" "$DESKTOP_DESTINATION" "$VERSION"
        else
            info "installed $DESKTOP_DESTINATION with the $EXPERIENCE experience"
        fi
        return 0
    fi

    downloads_dir="${HOME}/Downloads"
    mkdir -p "$downloads_dir" || die 'could not create the Downloads directory'
    DESKTOP_DESTINATION="$downloads_dir/$asset_name"
    install -m 644 "$candidate" "$DESKTOP_DESTINATION"
    write_desktop_preference
    if [ "$HEADLESS" -eq 1 ]; then
        printf '{"ok":true,"experience":"%s","installer":"%s","action":"open-dmg","version":"%s"}\n' \
            "$EXPERIENCE" "$DESKTOP_DESTINATION" "$VERSION"
    else
        info "verified $DESKTOP_DESTINATION with the $EXPERIENCE experience selected"
        if command -v open >/dev/null 2>&1; then
            open "$DESKTOP_DESTINATION" || die 'could not open the verified Henosis disk image'
        else
            info "open the disk image and drag Henosis to Applications"
        fi
    fi
}

# Download, verify, and activate the desktop distribution for one graphical profile.
install_desktop() {
    desktop_release_asset
    manifest="$WORK_DIR/SHA256SUMS"
    metadata="$WORK_DIR/release.json"
    candidate="$WORK_DIR/$DESKTOP_ASSET"
    url_base="${RELEASE_BASE%/}/$VERSION"
    metadata_url="${RELEASE_API%/}/$VERSION"
    info "verifying immutable release $VERSION"
    download "$metadata_url" "$metadata" || die 'could not download release metadata'
    verify_release_metadata "$metadata"
    info "downloading $DESKTOP_ASSET"
    download "$url_base/SHA256SUMS" "$manifest" || die 'could not download checksum manifest'
    download "$url_base/$DESKTOP_ASSET" "$candidate" || die "could not download $DESKTOP_ASSET"
    verify_archive "$manifest" "$candidate" "$DESKTOP_ASSET"
    verify_attestation "$candidate" "$DESKTOP_ASSET"
    install_desktop_candidate "$candidate" "$DESKTOP_ASSET"
}

# Remove only the private temporary workspace created by this installer.
cleanup() {
    rollback
    [ -n "${HENOSIS_STAGE:-}" ] && rm -f "$HENOSIS_STAGE"
    [ -n "${CRUCIBLE_STAGE:-}" ] && rm -f "$CRUCIBLE_STAGE"
    [ -n "${MANIFEST_STAGE:-}" ] && rm -f "$MANIFEST_STAGE"
    [ -n "${DESKTOP_STAGE:-}" ] && rm -f "$DESKTOP_STAGE"
    if [ "${ROLLBACK_FAILED:-0}" -eq 1 ]; then
        info "CRITICAL: rollback backups preserved at $WORK_DIR"
    elif [ -n "${WORK_DIR:-}" ]; then
        rm -rf "$WORK_DIR"
    fi
}

# Activate both verified executables and initialize Henosis as one transaction.
install_candidate() {
    henosis_candidate=$1
    crucible_candidate=$2
    target=$3
    HENOSIS_DESTINATION="$INSTALL_DIR/henosis"
    CRUCIBLE_DESTINATION="$INSTALL_DIR/crucible"
    MANIFEST_DESTINATION="$INSTALL_DIR/.henosis-installation"
    HENOSIS_BACKUP="$WORK_DIR/henosis.previous"
    CRUCIBLE_BACKUP="$WORK_DIR/crucible.previous"
    MANIFEST_BACKUP="$WORK_DIR/manifest.previous"
    HENOSIS_STAGE="$INSTALL_DIR/.henosis.new.$$"
    CRUCIBLE_STAGE="$INSTALL_DIR/.crucible.new.$$"
    MANIFEST_STAGE="$INSTALL_DIR/.henosis-installation.new.$$"
    HENOSIS_ACTIVATED=0
    CRUCIBLE_ACTIVATED=0
    MANIFEST_ACTIVATED=0
    if [ -f "$HENOSIS_DESTINATION" ]; then cp -p "$HENOSIS_DESTINATION" "$HENOSIS_BACKUP"; fi
    if [ -f "$CRUCIBLE_DESTINATION" ]; then cp -p "$CRUCIBLE_DESTINATION" "$CRUCIBLE_BACKUP"; fi
    if [ -f "$MANIFEST_DESTINATION" ]; then cp -p "$MANIFEST_DESTINATION" "$MANIFEST_BACKUP"; fi
    install -m 755 "$henosis_candidate" "$HENOSIS_STAGE"
    install -m 755 "$crucible_candidate" "$CRUCIBLE_STAGE"
    mv -f "$HENOSIS_STAGE" "$HENOSIS_DESTINATION"
    HENOSIS_ACTIVATED=1
    mv -f "$CRUCIBLE_STAGE" "$CRUCIBLE_DESTINATION"
    CRUCIBLE_ACTIVATED=1
    printf 'format=1\nexperience=cli\nversion=%s\n' "$VERSION" > "$MANIFEST_STAGE"
    chmod 600 "$MANIFEST_STAGE"
    mv -f "$MANIFEST_STAGE" "$MANIFEST_DESTINATION"
    MANIFEST_ACTIVATED=1
    if ! "$HENOSIS_DESTINATION" init --quick; then
        die 'henosis init --quick failed; the installer is rolling back the previous Henosis installation'
    fi
    HENOSIS_ACTIVATED=0
    CRUCIBLE_ACTIVATED=0
    MANIFEST_ACTIVATED=0
    rm -f "$HENOSIS_BACKUP" "$CRUCIBLE_BACKUP" "$MANIFEST_BACKUP"
    if [ "$HEADLESS" -eq 1 ]; then
        printf '{"ok":true,"experience":"cli","binary":"%s","crucible":"%s","version":"%s","target":"%s"}\n' "$HENOSIS_DESTINATION" "$CRUCIBLE_DESTINATION" "$VERSION" "$target"
    else
        info "installed $HENOSIS_DESTINATION and $CRUCIBLE_DESTINATION"
    fi
}

# Download or resolve, verify, activate, and initialize the native release.
main() {
    parse_args "$@"
    validate_experience
    case "$VERSION" in
        v[0-9]*)
            case "$VERSION" in *[!A-Za-z0-9._-]*) die 'release version contains unsupported characters' ;; esac
            ;;
        *) die 'release version must begin with v and contain a numeric version' ;;
    esac
    target=$(release_target)
    WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/henosis-install.XXXXXX") || die 'could not create temporary directory'
    trap cleanup EXIT HUP INT TERM
    if is_desktop_experience; then
        install_desktop
        return 0
    fi
    mkdir -p "$INSTALL_DIR"
    [ -d "$INSTALL_DIR" ] || die "install directory is not a directory: $INSTALL_DIR"
    ARCHIVE_BINARY=
    ARCHIVE_CRUCIBLE=
    if archive_binary "$target"; then
        candidate=$ARCHIVE_BINARY
        crucible_candidate=$ARCHIVE_CRUCIBLE
        info "installing the verified adjacent $VERSION binaries"
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
        verify_attestation "$archive" "$archive_name"
        tar -xzf "$archive" -C "$WORK_DIR" || die 'could not extract verified archive'
        candidate="$WORK_DIR/henosis-${VERSION#v}-${target}/henosis"
        crucible_candidate="$WORK_DIR/henosis-${VERSION#v}-${target}/crucible"
        [ -f "$candidate" ] && [ -x "$candidate" ] || die 'verified archive does not contain an executable henosis'
        [ -f "$crucible_candidate" ] && [ -x "$crucible_candidate" ] || die 'verified archive does not contain an executable crucible'
    fi
    install_candidate "$candidate" "$crucible_candidate" "$target"
}

main "$@"
