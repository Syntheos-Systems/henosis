#!/bin/sh
# Verify the static hardening contract for Henosis container assets.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

# Stop the container contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# Require a fixed line in one container asset.
require_line() { grep -F "$2" "$1" >/dev/null || fail "$1 is missing: $2"; }

dockerfile="$REPOSITORY_DIR/containers/Dockerfile"
local_compose="$REPOSITORY_DIR/containers/compose.local.yml"
production_compose="$REPOSITORY_DIR/containers/compose.production.yml"
production_environment="$REPOSITORY_DIR/containers/production.env.example"
for file in "$dockerfile" "$local_compose" "$production_compose" "$production_environment" "$REPOSITORY_DIR/.dockerignore"; do [ -f "$file" ] || fail "missing $file"; done
require_line "$dockerfile" 'USER 10001:10001'
require_line "$dockerfile" 'HEALTHCHECK'
require_line "$dockerfile" 'FROM rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0 AS build'
require_line "$dockerfile" 'FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818'
require_line "$dockerfile" 'COPY --from=build /src/target/release/henosis /usr/local/bin/henosis'
require_line "$dockerfile" 'ENTRYPOINT ["/usr/local/bin/henosis"]'
require_line "$local_compose" 'read_only: true'
require_line "$local_compose" 'cap_drop: [ALL]'
require_line "$local_compose" '127.0.0.1:8088:8088'
require_line "$local_compose" 'HENOSIS_AUTO_INIT: quick'
require_line "$production_compose" 'read_only: true'
require_line "$production_compose" 'image: ${HENOSIS_IMAGE_REPOSITORY:?Set HENOSIS_IMAGE_REPOSITORY}@sha256:${HENOSIS_IMAGE_DIGEST:?Set HENOSIS_IMAGE_DIGEST to the 64-character image digest}'
require_line "$production_compose" 'cap_drop: [ALL]'
require_line "$production_compose" 'no-new-privileges:true'
require_line "$production_compose" 'env_file: ["${HENOSIS_ENV_FILE:-.env.production}"]'
require_line "$production_compose" './secrets:/run/secrets/henosis:ro'
require_line "$production_environment" 'PHYLAXD_URL='
require_line "$production_environment" 'HENOSIS_IMAGE_REPOSITORY=ghcr.io/syntheos-systems/henosis'
require_line "$production_environment" 'HENOSIS_IMAGE_DIGEST=REPLACE_WITH_64_CHARACTER_LOWERCASE_HEX_DIGEST'
require_line "$production_environment" 'HENOSIS_WITNESS_URL='
require_line "$production_environment" 'HENOSIS_AUDIT_ORIGIN_KEY_FILE=/run/secrets/henosis/audit-origin.key'
require_line "$production_environment" 'HENOSIS_WITNESS_PUBLIC_KEY_FILE=/run/secrets/henosis/witness-public.key'
retired_prefix='SYNTHEOS_''PHYLAX_'
if grep -F "$retired_prefix" "$local_compose" "$production_compose" "$production_environment" >/dev/null; then
    fail 'container assets must not configure the retired in-process credential authority'
fi
for excluded in /data '**/.env' '**/secrets' '*.pem' '*.key' '*.p12' '*.pfx'; do
    require_line "$REPOSITORY_DIR/.dockerignore" "$excluded"
done
printf '%s\n' 'container contract passed'
