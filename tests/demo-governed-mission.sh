#!/bin/sh
# Exercise the governed mission client against deterministic HTTP fixtures.

set -eu

REPO_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/henosis-mission-test.XXXXXX")
ORIGINAL_PATH=$PATH
TENANT=11111111-1111-8111-8111-111111111111
PRINCIPAL=22222222-2222-8222-8222-222222222222
TASK_ID=33333333-3333-8333-8333-333333333333

# Remove only the temporary fixture directory created by this test.
cleanup() {
    rm -rf "$TEST_ROOT"
}

# Stop the test with a concise diagnostic.
fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

# Assert that a file contains a fixed string.
assert_contains() {
    file=$1
    expected=$2
    grep -F "$expected" "$file" >/dev/null || fail "$file does not contain: $expected"
}

# Create a deterministic curl double for every mission endpoint.
make_curl_fixture() {
    tools_dir=$1
    mkdir -p "$tools_dir"
    cat > "$tools_dir/curl" <<'EOF'
#!/bin/sh
set -eu
method=GET
body=
url=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --request)
            method=$2
            shift 2
            ;;
        --data)
            body=$2
            shift 2
            ;;
        --header|--connect-timeout|--max-time)
            shift 2
            ;;
        --silent|--show-error|--fail-with-body)
            shift
            ;;
        http://*)
            url=$1
            shift
            ;;
        *)
            printf 'unexpected curl argument: %s\n' "$1" >&2
            exit 2
            ;;
    esac
done
printf '%s %s\n' "$method" "$url" >> "${CURL_LOG:?}"
case "$method $url" in
    'GET http://127.0.0.1:8088/health')
        printf 'ok'
        ;;
    'POST http://127.0.0.1:8088/chiasm/tasks')
        printf '{"id":"%s","status":"active"}' \
            "${FAKE_TASK_ID:-33333333-3333-8333-8333-333333333333}"
        ;;
    'POST http://127.0.0.1:8088/dispatch')
        if printf '%s' "$body" | grep -F 'ignore previous instructions' >/dev/null; then
            gate=${FAKE_DENIED_GATE:-eidolon}
            printf '{"Denied":{"gate":"%s","reason":"hostile input"}}' "$gate"
        else
            printf '%s' '{"Executed":{"result":{"status":"ready","runtime":"henosis"}}}'
        fi
        ;;
    'GET http://127.0.0.1:8088/chiasm/tasks/33333333-3333-8333-8333-333333333333/activity?tenant=11111111-1111-8111-8111-111111111111&principal_id=22222222-2222-8222-8222-222222222222&limit=20')
        printf '%s' '[{"kind":"action.denied","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"kind":"action.invoked","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"kind":"action.completed","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"kind":"action.invoked","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}}]'
        ;;
    'GET http://127.0.0.1:8088/broca/actions?tenant=11111111-1111-8111-8111-111111111111&service=dispatcher&limit=20')
        printf '%s' '[{"action":"action.denied","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"action":"action.invoked","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"action":"action.completed","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}},{"action":"action.invoked","payload":{"task_id":"33333333-3333-8333-8333-333333333333","tool":"henosis","action":"probe"}}]'
        ;;
    *)
        printf 'unexpected fixture request: %s %s\n' "$method" "$url" >&2
        exit 22
        ;;
esac
EOF
    chmod 755 "$tools_dir/curl"
}

# Verify the complete happy path and its exact evidence summary.
test_success() {
    tools_dir="$TEST_ROOT/tools"
    make_curl_fixture "$tools_dir"
    CURL_LOG="$TEST_ROOT/curl.log" PATH="$tools_dir:$ORIGINAL_PATH" \
        "$REPO_DIR/scripts/demo-governed-mission.sh" \
        --base-url http://127.0.0.1:8088 \
        --tenant "$TENANT" \
        --principal "$PRINCIPAL" > "$TEST_ROOT/success.log" 2>&1
    assert_contains "$TEST_ROOT/success.log" 'Henosis governed mission passed.'
    assert_contains "$TEST_ROOT/success.log" "task: $TASK_ID"
    assert_contains "$TEST_ROOT/success.log" 'hostile input: henosis.probe -> denied by eidolon'
    [ "$(wc -l < "$TEST_ROOT/curl.log")" -eq 6 ] \
        || fail "mission did not perform the expected six HTTP requests"
}

# Verify a denial from the wrong authority fails the mission.
test_wrong_gate_fails() {
    tools_dir="$TEST_ROOT/tools"
    if CURL_LOG="$TEST_ROOT/wrong-gate-curl.log" FAKE_DENIED_GATE=phylaxd \
        PATH="$tools_dir:$ORIGINAL_PATH" "$REPO_DIR/scripts/demo-governed-mission.sh" \
        --base-url http://127.0.0.1:8088 \
        --tenant "$TENANT" \
        --principal "$PRINCIPAL" > "$TEST_ROOT/wrong-gate.log" 2>&1; then
        fail "mission accepted a denial from the wrong authority"
    fi
    assert_contains "$TEST_ROOT/wrong-gate.log" "denied by 'phylaxd', expected 'eidolon'"
}

# Verify the mission rejects targets outside its loopback route boundary before any request.
test_invalid_target_fails() {
    tools_dir="$TEST_ROOT/tools"
    for base_url in \
        http://example.com:8088 \
        http://user@127.0.0.1:8088 \
        http://127.0.0.1:8088/admin; do
        if CURL_LOG="$TEST_ROOT/invalid-target-curl.log" PATH="$tools_dir:$ORIGINAL_PATH" \
            "$REPO_DIR/scripts/demo-governed-mission.sh" \
            --base-url "$base_url" \
            --tenant "$TENANT" \
            --principal "$PRINCIPAL" > "$TEST_ROOT/invalid-target.log" 2>&1; then
            fail "mission accepted invalid target $base_url"
        fi
        assert_contains "$TEST_ROOT/invalid-target.log" \
            'base URL must be loopback HTTP'
    done
    [ ! -s "$TEST_ROOT/invalid-target-curl.log" ] \
        || fail "mission contacted an invalid target"
}

# Verify a malformed task identifier cannot alter later request paths.
test_invalid_task_id_fails() {
    tools_dir="$TEST_ROOT/tools"
    if CURL_LOG="$TEST_ROOT/invalid-task-curl.log" FAKE_TASK_ID='../broca/actions?tenant=other' \
        PATH="$tools_dir:$ORIGINAL_PATH" "$REPO_DIR/scripts/demo-governed-mission.sh" \
        --base-url http://127.0.0.1:8088 \
        --tenant "$TENANT" \
        --principal "$PRINCIPAL" > "$TEST_ROOT/invalid-task.log" 2>&1; then
        fail "mission accepted a malformed task identifier"
    fi
    assert_contains "$TEST_ROOT/invalid-task.log" 'invalid task UUID'
    [ "$(wc -l < "$TEST_ROOT/invalid-task-curl.log")" -eq 2 ] \
        || fail "mission continued after a malformed task identifier"
}

trap cleanup EXIT HUP INT TERM
test_success
test_wrong_gate_fails
test_invalid_target_fails
test_invalid_task_id_fails
printf 'PASS: governed mission client tests\n'
