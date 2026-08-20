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
production_roster="$REPOSITORY_DIR/containers/agents.production.example.toml"
readme="$REPOSITORY_DIR/README.md"
for file in "$dockerfile" "$local_compose" "$production_compose" "$production_environment" "$production_roster" "$readme" "$REPOSITORY_DIR/.dockerignore"; do [ -f "$file" ] || fail "missing $file"; done
require_line "$dockerfile" 'USER 10001:10001'
require_line "$dockerfile" 'COPY --from=build /src/target/release/crucible /usr/local/bin/crucible'
require_line "$dockerfile" 'HEALTHCHECK'
require_line "$dockerfile" 'FROM rust:1.94-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS build'
require_line "$dockerfile" 'apt-get install -y --no-install-recommends protobuf-compiler'
require_line "$dockerfile" 'FROM debian:bookworm-slim@sha256:7b140f374b289a7c2befc338f42ebe6441b7ea838a042bbd5acbfca6ec875818'
require_line "$dockerfile" 'COPY --from=build /src/target/release/henosis /usr/local/bin/henosis'
require_line "$dockerfile" 'ENTRYPOINT ["/usr/local/bin/henosis"]'
require_line "$local_compose" 'read_only: true'
require_line "$local_compose" 'cap_drop: [ALL]'
require_line "$local_compose" '127.0.0.1:8088:8088'
require_line "$local_compose" 'SYNTHEOS_AUTO_INIT: quick'
require_line "$production_compose" 'read_only: true'
require_line "$production_compose" 'image: ${SYNTHEOS_IMAGE_REPOSITORY:-${HENOSIS_IMAGE_REPOSITORY:?Set SYNTHEOS_IMAGE_REPOSITORY}}@sha256:${SYNTHEOS_IMAGE_DIGEST:-${HENOSIS_IMAGE_DIGEST:?Set SYNTHEOS_IMAGE_DIGEST to the 64-character image digest}}'
require_line "$production_compose" 'cap_drop: [ALL]'
require_line "$production_compose" 'no-new-privileges:true'
require_line "$production_compose" 'env_file: ["${SYNTHEOS_ENV_FILE:-${HENOSIS_ENV_FILE:-.env.production}}"]'
require_line "$production_compose" './secrets:/run/secrets/henosis:ro'
require_line "$production_environment" 'SYNTHEOS_IMAGE_REPOSITORY=ghcr.io/syntheos-systems/henosis'
require_line "$production_environment" 'SYNTHEOS_IMAGE_DIGEST=REPLACE_WITH_64_CHARACTER_LOWERCASE_HEX_DIGEST'
require_line "$production_environment" 'SYNTHEOS_ROOM_MODE=required'
require_line "$production_environment" 'SYNTHEOS_RIFT_BRIDGE_CONFIG=/run/secrets/henosis/agents.toml'
require_line "$production_environment" 'SYNTHEOS_RIFT_AGENT_JWT_SECRET=REPLACE_WITH_DIFFERENT_AGENT_ONLY_AT_LEAST_64_RANDOM_HEX_CHARACTERS'
require_line "$production_environment" 'SYNTHEOS_RIFT_ADDR=0.0.0.0:3200'
require_line "$production_environment" 'SYNTHEOS_RIFT_BRIDGE_ADDR=127.0.0.1:3201'
require_line "$production_environment" 'SYNTHEOS_RIFT_ALLOW_REMOTE_LISTEN=1'
require_line "$production_environment" 'SYNTHEOS_AUDIT_ORIGIN_KEY_FILE=/run/secrets/henosis/audit-origin.key'
require_line "$production_environment" 'SYNTHEOS_WITNESS_PUBLIC_KEY_FILE=/run/secrets/henosis/witness-public.key'
# Require every authority and managed-room setting enforced by production startup.
for required_environment_key in \
    SYNTHEOS_PLUTUS_DB \
    SYNTHEOS_OPERATOR_JWT_SECRET \
    SYNTHEOS_RIFT_JWT_SECRET \
    SYNTHEOS_RIFT_AGENT_JWT_SECRET \
    SYNTHEOS_RIFT_BRIDGE_SECRET \
    SYNTHEOS_RIFT_DATABASE_URL \
    SYNTHEOS_RIFT_BRIDGE_CONFIG \
    SYNTHEOS_RIFT_ADDR \
    SYNTHEOS_RIFT_BRIDGE_ADDR \
    SYNTHEOS_RIFT_ALLOW_REMOTE_LISTEN \
    SYNTHEOS_RIFT_CORS_ORIGINS \
    PHYLAXD_URL \
    HERMES_PHYLAXD_TOKEN \
    SYNTHEOS_WITNESS_URL \
    SYNTHEOS_AUDIT_ORIGIN_KEY_FILE \
    SYNTHEOS_AUDIT_ORIGIN_KEY_ID \
    SYNTHEOS_WITNESS_PUBLIC_KEY_FILE \
    SYNTHEOS_WITNESS_KEY_ID; do
    require_line "$production_environment" "$required_environment_key="
done
# Example values must make the three independent Rift authorities visually unambiguous.
human_jwt_placeholder=$(sed -n 's/^SYNTHEOS_RIFT_JWT_SECRET=//p' "$production_environment")
agent_jwt_placeholder=$(sed -n 's/^SYNTHEOS_RIFT_AGENT_JWT_SECRET=//p' "$production_environment")
bridge_placeholder=$(sed -n 's/^SYNTHEOS_RIFT_BRIDGE_SECRET=//p' "$production_environment")
if [ "$human_jwt_placeholder" = "$agent_jwt_placeholder" ] \
    || [ "$human_jwt_placeholder" = "$bridge_placeholder" ] \
    || [ "$agent_jwt_placeholder" = "$bridge_placeholder" ]; then
    fail 'production Rift human JWT, agent JWT, and bridge placeholders must differ pairwise'
fi
# The private bridge port must never appear in production publication metadata.
if grep -E '(^|[^0-9])3201([^0-9]|$)' "$production_compose" >/dev/null; then
    fail 'production compose must not publish or expose the Rift bridge port'
fi
require_line "$production_roster" '[[agents]]'
require_line "$production_roster" 'type = "Synapse"'
require_line "$production_roster" 'provider = "anthropic"'
require_line "$readme" 'containers/agents.production.example.toml'
# Keep deployment-owned room state and execution powers out of the starter roster.
if grep -E '^\[rift\]|^\[execution\]|^\[control\]|^\[pistis\]|^\[capabilities\]|^\[\[workspaces\]\]' "$production_roster" >/dev/null; then
    fail 'production roster must contain agent behavior only'
fi
retired_prefix='SYNTHEOS_''PHYLAX_'
if grep -F "$retired_prefix" "$local_compose" "$production_compose" "$production_environment" "$production_roster" >/dev/null; then
    fail 'container assets must not configure the retired in-process credential authority'
fi
for excluded in /data '**/.env' '**/secrets' '*.pem' '*.key' '*.p12' '*.pfx'; do
    require_line "$REPOSITORY_DIR/.dockerignore" "$excluded"
done
printf '%s\n' 'container contract passed'
