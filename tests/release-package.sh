#!/bin/sh
# Verify the Unix native archive contains only the public Henosis launch contract.

set -eu

REPOSITORY_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
TEST_DIRECTORY=$(mktemp -d "${TMPDIR:-/tmp}/henosis-package-test.XXXXXX")

# Remove only the package test workspace.
cleanup() { rm -rf "$TEST_DIRECTORY"; }

# Stop the release contract with a diagnostic.
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

trap cleanup EXIT HUP INT TERM
binary="$TEST_DIRECTORY/henosis"; printf '#!/bin/sh\nexit 0\n' > "$binary"; chmod 755 "$binary"
crucible="$TEST_DIRECTORY/crucible"; printf '#!/bin/sh\nexit 0\n' > "$crucible"; chmod 755 "$crucible"
SOURCE_DATE_EPOCH=1784768092 "$REPOSITORY_DIR/scripts/package-release.sh" "$binary" "$crucible" 0.1.0-alpha.6 x86_64-unknown-linux-musl "$TEST_DIRECTORY/dist" >/dev/null
archive="$TEST_DIRECTORY/dist/henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl.tar.gz"
[ -f "$archive" ] || fail 'archive was not created'
members=$(tar -tzf "$archive")
expected='henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/HENOSIS_ARCHIVE
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/LICENSE
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/README.md
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/crucible
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/henosis
henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/install.sh'
[ "$members" = "$expected" ] || fail 'archive members differ from contract'
tar -xOf "$archive" henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/README.md | grep -F 'henosis init --quick' >/dev/null || fail 'archive does not document initialization'
tar -xOf "$archive" henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/README.md | grep -F 'offline installation' >/dev/null || fail 'archive does not document offline installation'
tar -xOf "$archive" henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/README.md | grep -F 'crucible' >/dev/null || fail 'archive does not document Crucible'
[ "$(tar -xOf "$archive" henosis-0.1.0-alpha.6-x86_64-unknown-linux-musl/HENOSIS_ARCHIVE)" = 'v0.1.0-alpha.6 x86_64-unknown-linux-musl' ] || fail 'archive marker is incorrect'
grep -F 'aarch64-unknown-linux-musl' "$REPOSITORY_DIR/.github/workflows/ci.yml" >/dev/null || fail 'workflow lacks Linux arm64'
grep -F 'henosis init --quick' "$REPOSITORY_DIR/install.sh" >/dev/null || fail 'installer lacks initialization contract'

# Exercise the real promotion path with deterministic GitHub and Git boundaries.
fake_bin="$TEST_DIRECTORY/fake-bin"
promotion_log="$TEST_DIRECTORY/promotion.log"
candidate_sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
main_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
mkdir "$fake_bin"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$*" in' \
    '    *check-runs*)' \
    '        printf "Rust quality\tcompleted\tsuccess\n"' \
    '        case "${FAKE_DESKTOP_STATUS:-success}" in' \
    '            success) printf "Desktop quality\tcompleted\tsuccess\n" ;;' \
    '            failure) printf "Desktop quality\tcompleted\tfailure\n" ;;' \
    '        esac' \
    '        case "${FAKE_COGNITION_STATUS:-missing}" in' \
    '            success) printf "Cognition quality\tcompleted\tsuccess\n" ;;' \
    '            failure) printf "Cognition quality\tcompleted\tfailure\n" ;;' \
    '        esac' \
    '        printf "Dependency audit\tcompleted\tsuccess\n"' \
    '        printf "Secret scan\tcompleted\tsuccess\n"' \
    '        printf "Release package contract\tcompleted\tsuccess\n"' \
    '        printf "Windows release package contract\tcompleted\tsuccess\n"' \
    '        ;;' \
    '    *)' \
    '        grep -F "push origin $FAKE_CANDIDATE_SHA:refs/heads/candidate/$FAKE_CANDIDATE_SHA" "$PROMOTION_LOG" >/dev/null || exit 70' \
    '        printf "%s\n" "${FAKE_GITHUB_ACTOR:-Ghost-Frame}"' \
    '        ;;' \
    'esac' > "$fake_bin/gh"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in' \
    '    rev-parse) printf "%s\n" "$FAKE_CANDIDATE_SHA" ;;' \
    '    show) printf "%s\n" "GhostFrame <ghostframe@girbox.org>" ;;' \
    '    verify-commit) [ "${FAKE_SIGNATURE_STATUS:-valid}" = valid ] ;;' \
    '    push)' \
    '        printf "%s\n" "$*" >> "$PROMOTION_LOG"' \
    '        case "$*" in' \
    '            *refs/heads/main) [ "${FAKE_MAIN_PUSH_STATUS:-success}" = success ] ;;' \
    '        esac' \
    '        ;;' \
    '    ls-remote)' \
    '        [ "${FAKE_MAIN_LOOKUP_STATUS:-success}" = success ] || exit 1' \
    '        if [ "${FAKE_MAIN_REF_STATUS:-present}" = present ]; then' \
    '            printf "%s\trefs/heads/main\n" "$FAKE_MAIN_SHA"' \
    '        fi' \
    '        ;;' \
    '    merge-base) [ "${FAKE_FAST_FORWARD_STATUS:-success}" = success ] ;;' \
    '    *) exit 1 ;;' \
    'esac' > "$fake_bin/git"
printf '%s\n' '#!/bin/sh' 'printf "sleep %s\n" "$*" >> "$PROMOTION_LOG"' > "$fake_bin/sleep"
chmod 755 "$fake_bin/gh" "$fake_bin/git" "$fake_bin/sleep"

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" PROMOTION_CHECK_ATTEMPTS=0 \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-configuration.log" 2>&1
then
    fail 'promotion accepted an invalid polling budget'
fi
grep -F 'PROMOTION_CHECK_ATTEMPTS must be a positive integer' "$TEST_DIRECTORY/promotion-configuration.log" >/dev/null || fail 'polling budget validation failed for the wrong reason'
if grep -F 'refs/heads/candidate/' "$promotion_log" >/dev/null; then
    fail 'promotion pushed a candidate with an invalid polling budget'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" PROMOTION_CHECK_INTERVAL_SECONDS=0 \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-interval.log" 2>&1
then
    fail 'promotion accepted an invalid polling interval'
fi
grep -F 'PROMOTION_CHECK_INTERVAL_SECONDS must be a positive integer' "$TEST_DIRECTORY/promotion-interval.log" >/dev/null || fail 'polling interval validation failed for the wrong reason'
if grep -F 'refs/heads/candidate/' "$promotion_log" >/dev/null; then
    fail 'promotion pushed a candidate with an invalid polling interval'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_SIGNATURE_STATUS=invalid \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-signature.log" 2>&1
then
    fail 'promotion accepted an untrusted local signature'
fi
grep -F 'lacks a trusted GhostFrame signature' "$TEST_DIRECTORY/promotion-signature.log" >/dev/null || fail 'local signature check failed for the wrong reason'
if grep -F 'refs/heads/candidate/' "$promotion_log" >/dev/null; then
    fail 'promotion pushed an untrusted local candidate'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_GITHUB_ACTOR=Other-User \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-actor.log" 2>&1
then
    fail 'promotion accepted the wrong GitHub-attributed actor'
fi
grep -F 'is not attributed to Ghost-Frame' "$TEST_DIRECTORY/promotion-actor.log" >/dev/null || fail 'GitHub actor check failed for the wrong reason'
grep -F "push origin $candidate_sha:refs/heads/candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not publish the isolated candidate before remote attribution'
grep -F 'push origin --delete candidate/' "$promotion_log" >/dev/null || fail 'promotion did not remove the rejected candidate'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed main for the wrong GitHub-attributed actor'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=missing PROMOTION_CHECK_ATTEMPTS=2 PROMOTION_CHECK_INTERVAL_SECONDS=1 \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-missing.log" 2>&1
then
    fail 'promotion accepted a candidate without Cognition quality'
fi
grep -F 'required checks did not pass' "$TEST_DIRECTORY/promotion-missing.log" >/dev/null || fail 'promotion failed for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate with a missing check'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed main without Cognition quality'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=failure \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-failure.log" 2>&1
then
    fail 'promotion accepted a failed Cognition quality check'
fi
grep -F 'required checks did not pass' "$TEST_DIRECTORY/promotion-failure.log" >/dev/null || fail 'terminal check failure stopped promotion for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate with a failed check'
if grep -F 'sleep ' "$promotion_log" >/dev/null; then
    fail 'promotion waited after a terminal check failure'
fi
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed main after a failed Cognition quality check'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success FAKE_DESKTOP_STATUS=missing PROMOTION_CHECK_ATTEMPTS=2 PROMOTION_CHECK_INTERVAL_SECONDS=1 \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-desktop-missing.log" 2>&1
then
    fail 'promotion accepted a candidate without Desktop quality'
fi
grep -F 'required checks did not pass' "$TEST_DIRECTORY/promotion-desktop-missing.log" >/dev/null || fail 'missing Desktop quality stopped promotion for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate with missing Desktop quality'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed main without Desktop quality'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success FAKE_MAIN_LOOKUP_STATUS=failure \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-main-lookup.log" 2>&1
then
    fail 'promotion accepted an unreadable main reference'
fi
grep -F 'main reference could not be read' "$TEST_DIRECTORY/promotion-main-lookup.log" >/dev/null || fail 'main lookup failure stopped promotion for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate after a main lookup failure'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed an unreadable main reference'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success FAKE_MAIN_REF_STATUS=missing \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-main-missing.log" 2>&1
then
    fail 'promotion accepted a missing main reference'
fi
grep -F 'main reference is missing' "$TEST_DIRECTORY/promotion-main-missing.log" >/dev/null || fail 'missing main reference stopped promotion for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate after a missing main reference'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion created a missing main reference'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success FAKE_FAST_FORWARD_STATUS=failure \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-fast-forward.log" 2>&1
then
    fail 'promotion accepted a non-fast-forward candidate'
fi
grep -F 'candidate is not a fast-forward of main' "$TEST_DIRECTORY/promotion-fast-forward.log" >/dev/null || fail 'fast-forward validation stopped promotion for the wrong reason'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the non-fast-forward candidate'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed a non-fast-forward candidate to main'
fi

: > "$promotion_log"
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success FAKE_MAIN_PUSH_STATUS=failure \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-main-push.log" 2>&1
then
    fail 'promotion reported success after the main push failed'
fi
grep -F 'main reference could not be updated' "$TEST_DIRECTORY/promotion-main-push.log" >/dev/null || fail 'main push failure stopped promotion for the wrong reason'
grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null || fail 'promotion did not attempt the validated main update'
grep -F "push origin --delete candidate/$candidate_sha" "$promotion_log" >/dev/null || fail 'promotion did not remove the candidate after the main push failed'

: > "$promotion_log"
PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-success.log" 2>&1 ||
    fail 'promotion rejected seven successful required checks'
grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null || fail 'promotion did not push main after seven successful checks'
printf '%s\n' 'release package contract passed'
