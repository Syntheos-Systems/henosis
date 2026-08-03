#!/bin/sh
# Run Rift's cursor-contract test against a fresh disposable PostgreSQL server.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)

exec "$REPOSITORY_DIR/tests/with-rift-test-postgres.sh" \
    cargo test --locked -p henosis-rift-server \
    routes::messages::tests::list_messages_enforce_live_channel_cursor_contracts \
    -- --exact --nocapture
