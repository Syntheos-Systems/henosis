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
SOURCE_DATE_EPOCH=1784768092 "$REPOSITORY_DIR/scripts/package-release.sh" "$binary" 0.1.0-alpha.1 x86_64-unknown-linux-musl "$TEST_DIRECTORY/dist" >/dev/null
archive="$TEST_DIRECTORY/dist/henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl.tar.gz"
[ -f "$archive" ] || fail 'archive was not created'
members=$(tar -tzf "$archive")
expected='henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/HENOSIS_ARCHIVE
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/LICENSE
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/README.md
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/henosis
henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/install.sh'
[ "$members" = "$expected" ] || fail 'archive members differ from contract'
tar -xOf "$archive" henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/README.md | grep -F 'henosis init --quick' >/dev/null || fail 'archive does not document initialization'
tar -xOf "$archive" henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/README.md | grep -F 'offline installation' >/dev/null || fail 'archive does not document offline installation'
[ "$(tar -xOf "$archive" henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl/HENOSIS_ARCHIVE)" = 'v0.1.0-alpha.1 x86_64-unknown-linux-musl' ] || fail 'archive marker is incorrect'
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
    '        [ "${FAKE_COGNITION_STATUS:-missing}" = success ] && printf "Cognition quality\tcompleted\tsuccess\n"' \
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
    '    push) printf "%s\n" "$*" >> "$PROMOTION_LOG" ;;' \
    '    ls-remote) printf "%s\trefs/heads/main\n" "$FAKE_MAIN_SHA" ;;' \
    '    merge-base) exit 0 ;;' \
    '    *) exit 1 ;;' \
    'esac' > "$fake_bin/git"
printf '%s\n' '#!/bin/sh' 'exit 0' > "$fake_bin/sleep"
chmod 755 "$fake_bin/gh" "$fake_bin/git" "$fake_bin/sleep"

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
if PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=missing \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-missing.log" 2>&1
then
    fail 'promotion accepted a candidate without Cognition quality'
fi
grep -F 'required checks did not pass' "$TEST_DIRECTORY/promotion-missing.log" >/dev/null || fail 'promotion failed for the wrong reason'
if grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null; then
    fail 'promotion pushed main without Cognition quality'
fi

: > "$promotion_log"
PATH="$fake_bin:$PATH" PROMOTION_LOG="$promotion_log" FAKE_CANDIDATE_SHA="$candidate_sha" FAKE_MAIN_SHA="$main_sha" FAKE_COGNITION_STATUS=success \
    "$REPOSITORY_DIR/scripts/promote-main.sh" "$candidate_sha" >"$TEST_DIRECTORY/promotion-success.log" 2>&1 ||
    fail 'promotion rejected six successful required checks'
grep -F "push origin $candidate_sha:refs/heads/main" "$promotion_log" >/dev/null || fail 'promotion did not push main after six successful checks'
printf '%s\n' 'release package contract passed'
