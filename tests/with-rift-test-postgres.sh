#!/bin/sh
# Run one command against an isolated PostgreSQL instance owned by this harness.

set -eu
umask 077

CONTAINER_ENGINE=${HENOSIS_TEST_CONTAINER_ENGINE:-}
POSTGRES_IMAGE=${HENOSIS_TEST_POSTGRES_IMAGE:-docker.io/library/postgres@sha256:0fc5c901ec0a3c55ce70b99b040daeb89d5b35b61febbced1b4b24dbc3153ec8}
POSTGRES_DATABASE=henosis_test
RUN_ID="$(date +%s)-$$"
CONTAINER_NAME="henosis-rift-postgres-$RUN_ID"
OWNERSHIP_LABEL=systems.syntheos.henosis.test-run
container_id=

# Stop with one user-facing diagnostic.
fail() {
    printf 'with-rift-test-postgres: %s\n' "$*" >&2
    exit 1
}

# Select an explicit engine or the first supported local engine.
select_container_engine() {
    if [ -n "$CONTAINER_ENGINE" ]; then
        command -v "$CONTAINER_ENGINE" >/dev/null 2>&1 ||
            fail "container engine not found: $CONTAINER_ENGINE"
        return
    fi

    for candidate in podman docker; do
        if command -v "$candidate" >/dev/null 2>&1; then
            CONTAINER_ENGINE=$candidate
            return
        fi
    done

    fail 'neither podman nor docker is installed'
}

# Print bounded database diagnostics without exposing child environments.
print_database_logs() {
    [ -n "$container_id" ] || return 0
    "$CONTAINER_ENGINE" logs --tail 100 "$container_id" >&2 2>/dev/null || true
}

# Remove only the container carrying this invocation's ownership label.
cleanup() {
    cleanup_status=$?
    trap - EXIT HUP INT TERM
    set +e

    if [ -n "$container_id" ]; then
        if "$CONTAINER_ENGINE" inspect "$container_id" >/dev/null 2>&1; then
            actual_label=$(
                "$CONTAINER_ENGINE" inspect \
                    --format "{{ index .Config.Labels \"$OWNERSHIP_LABEL\" }}" \
                    "$container_id" 2>/dev/null
            )
            if [ "$actual_label" = "$RUN_ID" ]; then
                if ! "$CONTAINER_ENGINE" rm --force --volumes "$container_id" >/dev/null; then
                    printf '%s\n' \
                        'with-rift-test-postgres: failed to remove the owned container and volume' \
                        >&2
                    if [ "$cleanup_status" -eq 0 ]; then
                        cleanup_status=1
                    fi
                fi
            else
                printf '%s\n' \
                    "with-rift-test-postgres: refusing to remove a container without the expected ownership label" \
                    >&2
                if [ "$cleanup_status" -eq 0 ]; then
                    cleanup_status=1
                fi
            fi
        else
            printf '%s\n' \
                'with-rift-test-postgres: could not verify the created container during cleanup' \
                >&2
            if [ "$cleanup_status" -eq 0 ]; then
                cleanup_status=1
            fi
        fi
    fi

    exit "$cleanup_status"
}

# Convert termination signals into stable command statuses before cleanup.
handle_signal() {
    signal_status=$1
    exit "$signal_status"
}

# Wait until PostgreSQL accepts connections or the bounded deadline expires.
wait_for_postgres() {
    attempts=0
    while [ "$attempts" -lt 60 ]; do
        if "$CONTAINER_ENGINE" exec "$container_id" \
            pg_isready --quiet --host 127.0.0.1 --port 5432 \
            --username postgres --dbname "$POSTGRES_DATABASE"; then
            return 0
        fi
        if [ "$("$CONTAINER_ENGINE" inspect --format '{{.State.Running}}' "$container_id" 2>/dev/null)" != true ]; then
            print_database_logs
            fail 'PostgreSQL exited before becoming ready'
        fi
        attempts=$((attempts + 1))
        sleep 1
    done

    print_database_logs
    fail 'PostgreSQL did not become ready within 60 seconds'
}

[ "$#" -gt 0 ] || fail 'usage: with-rift-test-postgres.sh COMMAND [ARGUMENT ...]'
select_container_engine

trap cleanup EXIT
trap 'handle_signal 129' HUP
trap 'handle_signal 130' INT
trap 'handle_signal 143' TERM

container_id=$(
    "$CONTAINER_ENGINE" run --detach \
        --name "$CONTAINER_NAME" \
        --label "$OWNERSHIP_LABEL=$RUN_ID" \
        --env POSTGRES_DB="$POSTGRES_DATABASE" \
        --env POSTGRES_HOST_AUTH_METHOD=trust \
        --publish 127.0.0.1::5432 \
        "$POSTGRES_IMAGE"
)

wait_for_postgres
published_address=$(
    "$CONTAINER_ENGINE" port "$container_id" 5432/tcp | sed -n '1p'
)
published_port=${published_address##*:}
case "$published_port" in
    ''|*[!0-9]*) fail "could not resolve PostgreSQL port from: $published_address" ;;
esac

DATABASE_URL="postgresql://postgres@127.0.0.1:$published_port/$POSTGRES_DATABASE"
HENOSIS_RIFT_TEST_DATABASE_URL=$DATABASE_URL
export DATABASE_URL HENOSIS_RIFT_TEST_DATABASE_URL

set +e
"$@"
child_status=$?
set -e
exit "$child_status"
