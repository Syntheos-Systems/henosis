#!/bin/sh
# Verify the static release authority, provenance, and immutable bootstrap contract.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
WORKFLOW="$REPOSITORY_DIR/.github/workflows/ci.yml"
README="$REPOSITORY_DIR/README.md"
ALLOWED_SIGNERS="$REPOSITORY_DIR/security/release-allowed-signers"
EXPECTED_SIGNER='ghostframe@girbox.org namespaces="git" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA8RT1QrONYawdO9XOD1sjgy0cOewtktEBm7gZniJ0/o'
BOOTSTRAP_COMMIT=1a9ff0730f36e9a3af537e09177d36e3be204229

# Stop the release trust contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Require a fixed trust-contract line in one file.
require_line() { grep -F -- "$2" "$1" >/dev/null || fail "$1 is missing: $2"; }

[ -f "$ALLOWED_SIGNERS" ] || fail 'release allowed-signers file is missing'
[ "$(cat "$ALLOWED_SIGNERS")" = "$EXPECTED_SIGNER" ] ||
    fail 'release allowed-signers file must contain only the GhostFrame release key'
[ "$(grep -Fc 'git verify-tag "$GITHUB_REF_NAME"' "$WORKFLOW")" -eq 1 ] ||
    fail 'release authority must be verified inside the protected publication job'
[ "$(grep -Fc 'git merge-base --is-ancestor "$tag_commit" refs/remotes/origin/main' "$WORKFLOW")" -eq 2 ] ||
    fail 'release ancestry must be verified before build and publication'
require_line "$WORKFLOW" 'environment: release'
[ "$(grep -Fc "if: github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')" "$WORKFLOW")" -eq 3 ] ||
    fail 'release jobs must run only for v-tag push events'
if grep -F 'workflow_dispatch:' "$WORKFLOW" >/dev/null; then
    fail 'release workflow must not expose a manual dispatch path'
fi
require_line "$WORKFLOW" 'RELEASE_ALLOWED_SIGNERS: ${{ vars.RELEASE_ALLOWED_SIGNERS }}'
require_line "$WORKFLOW" 'gpg.ssh.allowedSignersFile "$RUNNER_TEMP/release-allowed-signers"'
require_line "$WORKFLOW" 'attestations: write'
require_line "$WORKFLOW" 'id-token: write'
require_line "$WORKFLOW" 'actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1'
require_line "$WORKFLOW" 'actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1'
require_line "$WORKFLOW" 'actions/attest@f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6 # v4.2.0'
require_line "$WORKFLOW" 'anchore/sbom-action@e22c389904149dbc22b58101806040fa8d37a610 # v0.24.0'
require_line "$WORKFLOW" 'docker/setup-qemu-action@96fe6ef7f33517b61c61be40b68a1882f3264fb8 # v4.2.0'
require_line "$WORKFLOW" 'docker/setup-buildx-action@bb05f3f5519dd87d3ba754cc423b652a5edd6d2c # v4.2.0'
require_line "$WORKFLOW" 'docker/login-action@06fb636fac595d6fb4b28a5dfcb21a6f5091859c # v4.5.0'
require_line "$WORKFLOW" 'docker/build-push-action@53b7df96c91f9c12dcc8a07bcb9ccacbed38856a # v7.3.0'
require_line "$WORKFLOW" 'subject-checksums: release/SHA256SUMS'
require_line "$WORKFLOW" 'push-to-registry: true'
require_line "$WORKFLOW" 'HENOSIS_IMAGE_REFERENCE=%s@sha256:%s'
require_line "$WORKFLOW" 'container-image.env'
require_line "$WORKFLOW" '--generate-notes --verify-tag'
require_line "$WORKFLOW" 'v0.1.0-alpha.3 was not published because its release workflow failed before asset upload.'
require_line "$WORKFLOW" '--notes-start-tag v0.1.0-alpha.2'
require_line "$WORKFLOW" 'git ls-remote --exit-code --tags origin "refs/tags/$GITHUB_REF_NAME"'
require_line "$WORKFLOW" '--jq .immutable'
if grep -F 'repos/$GITHUB_REPOSITORY/immutable-releases' "$WORKFLOW" >/dev/null; then
    fail 'workflow token cannot read the immutable release administration endpoint'
fi
require_line "$REPOSITORY_DIR/install.sh" 'release_metadata_fields'
require_line "$REPOSITORY_DIR/install.sh" 'VERSION=${HENOSIS_VERSION:-v0.1.0-alpha.6}'
require_line "$REPOSITORY_DIR/install.ps1" '$metadata.immutable -ne $true'
require_line "$REPOSITORY_DIR/install.ps1" "else { 'v0.1.0-alpha.6' }"
require_line "$REPOSITORY_DIR/crates/syntheos-server/Cargo.toml" 'version = "0.1.0-alpha.6"'
require_line "$REPOSITORY_DIR/Cargo.lock" 'version = "0.1.0-alpha.6"'
require_line "$README" "https://raw.githubusercontent.com/Syntheos-Systems/henosis/$BOOTSTRAP_COMMIT/install.sh"
require_line "$README" "https://raw.githubusercontent.com/Syntheos-Systems/henosis/$BOOTSTRAP_COMMIT/install.ps1"
require_line "$README" '| sh -s -- --version v0.1.0-alpha.6'
require_line "$README" '-OutFile $installer'
require_line "$README" '$powerShell = (Get-Process -Id $PID).Path'
require_line "$README" '& $powerShell -NoProfile -ExecutionPolicy Bypass -File $installer -Version'
require_line "$README" 'if ($LASTEXITCODE -ne 0)'
require_line "$README" "-Version 'v0.1.0-alpha.6'"
if sed -n 's/.*uses:[[:space:]]*[^@]*@\([^ #]*\).*/\1/p' "$WORKFLOW" |
    grep -Ev '^[0-9a-f]{40}$' >/dev/null; then
    fail 'workflow contains an action that is not pinned to a full commit'
fi
if [ "$(grep -Ec 'raw\.githubusercontent\.com/Syntheos-Systems/henosis/[0-9a-f]{40}/install\.(sh|ps1)' "$README")" -ne 2 ]; then
    fail 'README must contain exactly one full-commit bootstrap URL per installer'
fi
if grep -E 'raw\.githubusercontent\.com/Syntheos-Systems/henosis/[^/]+/install\.(sh|ps1)' "$README" |
    grep -Ev 'raw\.githubusercontent\.com/Syntheos-Systems/henosis/[0-9a-f]{40}/install\.(sh|ps1)' >/dev/null; then
    fail 'README executes an installer that is not pinned to a full commit'
fi
printf '%s\n' 'release trust contract passed'
