#!/bin/sh
# Verify vendor metadata modes fail closed without requiring private upstream checkouts.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/henosis-vendor-test.XXXXXX")

# Remove only the vendor metadata test workspace.
cleanup() { rm -rf "$TEST_ROOT"; }

# Stop the vendor metadata contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Calculate the fixture component digest using the production NUL-framed record format.
fixture_content_digest() {
    fixture=$1
    {
        while IFS= read -r path; do
            entry="$(git -C "$fixture" ls-files -s -- "$path")"
            mode="${entry%% *}"
            remainder="${entry#* }"
            object="${remainder%% *}"
            blob_sha256="$(git -C "$fixture" cat-file blob "$object" | sha256sum | awk '{print $1}')"
            printf '%s\0%s\0%s\0' "$mode" "$path" "$blob_sha256"
        done <<EOF
$(git -C "$fixture" ls-files -- vendor/component ":(exclude)vendor/component/VENDOR.md")
EOF
    } | sha256sum | awk '{print $1}'
}

# Create an isolated drift-checker fixture with one metadata document.
make_fixture() {
    fixture=$1
    mode=$2
    mkdir -p "$fixture/scripts" "$fixture/vendor/component/source"
    cp "$REPOSITORY_DIR/scripts/vendor-drift.sh" "$fixture/scripts/vendor-drift.sh"
    printf '%s\n' 'fixture content' > "$fixture/vendor/component/source/payload"
    git -C "$fixture" init -q
    git -C "$fixture" add vendor/component/source/payload
    content_sha256="$(fixture_content_digest "$fixture")"
    {
        printf '%s\n' "# Fixture"
        printf 'Mode: %s\n' "$mode"
        printf '%s\n' 'Pin: 0123456789abcdef0123456789abcdef01234567'
        if [ "$mode" = "PRISTINE" ]; then
            printf '%s\n' 'Upstream: frameshift'
        elif [ "$mode" = "PATCHED" ]; then
            printf '%s\n' 'Upstream: kleos'
        else
            printf '%s\n' 'Upstream: crates.io:fixture'
        fi
        printf 'Content-SHA256: %s\n' "$content_sha256"
        if [ "$mode" != "OWNED" ]; then printf '%s\n' 'Mirror: source=source'; fi
    } > "$fixture/vendor/component/VENDOR.md"
    git -C "$fixture" add vendor/component/VENDOR.md
}

# Verify every documented vendor mode passes metadata-only validation.
test_supported_modes() {
    for mode in PRISTINE PATCHED OWNED; do
        fixture="$TEST_ROOT/supported-$mode"
        make_fixture "$fixture" "$mode"
        "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result"
        grep -F "META  $fixture/vendor/component: $mode" "$fixture/result" >/dev/null ||
            fail "$mode metadata was not checked"
    done
}

# Verify a patched component without a mirror mapping is rejected.
test_patched_requires_mirror() {
    fixture="$TEST_ROOT/missing-mirror"
    make_fixture "$fixture" OWNED
    sed -e 's/Mode: OWNED/Mode: PATCHED/' -e 's/Upstream: crates.io:fixture/Upstream: kleos/' \
        "$fixture/vendor/component/VENDOR.md" > "$fixture/vendor/component/VENDOR.invalid"
    mv "$fixture/vendor/component/VENDOR.invalid" "$fixture/vendor/component/VENDOR.md"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'PATCHED metadata without a Mirror field succeeded'
    fi
    grep -F 'PATCHED components require a Mirror field' "$fixture/result" >/dev/null ||
        fail 'missing PATCHED mirror did not report the metadata error'
}

# Verify tracked metadata rejects ambiguous fields, unsupported upstreams, and partial pins.
test_malformed_tracked_metadata() {
    fixture="$TEST_ROOT/duplicate-mode"
    make_fixture "$fixture" PRISTINE
    printf '%s\n' 'Mode: OWNED' >> "$fixture/vendor/component/VENDOR.md"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'duplicate Mode metadata succeeded'
    fi

    fixture="$TEST_ROOT/unsupported-upstream"
    make_fixture "$fixture" PRISTINE
    sed 's/Upstream: frameshift/Upstream: unknown/' "$fixture/vendor/component/VENDOR.md" > "$fixture/vendor/component/VENDOR.invalid"
    mv "$fixture/vendor/component/VENDOR.invalid" "$fixture/vendor/component/VENDOR.md"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'unsupported tracked upstream succeeded'
    fi

    fixture="$TEST_ROOT/partial-pin"
    make_fixture "$fixture" PATCHED
    sed 's/0123456789abcdef0123456789abcdef01234567/0123456789abcdef/' "$fixture/vendor/component/VENDOR.md" > "$fixture/vendor/component/VENDOR.invalid"
    mv "$fixture/vendor/component/VENDOR.invalid" "$fixture/vendor/component/VENDOR.md"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'partial tracked pin succeeded'
    fi
}

# Verify changed component content cannot pass with the previously reviewed digest.
test_content_digest_mismatch() {
    fixture="$TEST_ROOT/content-mismatch"
    make_fixture "$fixture" PRISTINE
    printf '%s\n' 'changed content' > "$fixture/vendor/component/source/payload"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'changed component content succeeded with a stale digest'
    fi
    grep -F 'tracked component content differs from the index' "$fixture/result" >/dev/null ||
        fail 'changed component content did not report the digest gate'
}

# Verify removing every vendor metadata document cannot silently disable the gate.
test_missing_metadata_fails() {
    fixture="$TEST_ROOT/no-metadata"
    mkdir -p "$fixture/scripts" "$fixture/vendor"
    cp "$REPOSITORY_DIR/scripts/vendor-drift.sh" "$fixture/scripts/vendor-drift.sh"
    if "$fixture/scripts/vendor-drift.sh" --metadata-only > "$fixture/result" 2>&1; then
        fail 'empty vendor metadata set succeeded'
    fi
    grep -F 'no VENDOR.md metadata files found' "$fixture/result" >/dev/null ||
        fail 'empty vendor metadata set did not report the error'
}

trap cleanup EXIT HUP INT TERM
test_supported_modes
test_patched_requires_mirror
test_malformed_tracked_metadata
test_content_digest_mismatch
test_missing_metadata_fails
printf '%s\n' 'vendor metadata contract passed'
