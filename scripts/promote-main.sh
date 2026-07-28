#!/bin/sh
# Promote one exact validated candidate SHA to main through the GitHub checks API.

set -eu

PROGRAM=promote-main
REPOSITORY=${GITHUB_REPOSITORY:-Syntheos-Systems/henosis}
CHECK_ATTEMPTS=${PROMOTION_CHECK_ATTEMPTS:-361}
CHECK_INTERVAL_SECONDS=${PROMOTION_CHECK_INTERVAL_SECONDS:-10}

# Stop promotion with a precise diagnostic.
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

# Require the GitHub CLI before changing any remote references.
require_gh() { command -v gh >/dev/null 2>&1 || die 'gh CLI is required'; }

# Require a positive integer configuration value.
require_positive_integer() {
    name=$1
    value=$2
    case "$value" in
        '' | *[!0-9]*) die "$name must be a positive integer" ;;
    esac
    [ "$value" -gt 0 ] 2>/dev/null || die "$name must be a positive integer"
}

# Verify the local candidate has the exact trusted identity and signature.
verify_local_identity() {
    sha=$1
    [ "$(git show -s --format='%an <%ae>' "$sha")" = 'GhostFrame <ghostframe@girbox.org>' ] ||
        die "candidate $sha has the wrong author identity"
    [ "$(git show -s --format='%cn <%ce>' "$sha")" = 'GhostFrame <ghostframe@girbox.org>' ] ||
        die "candidate $sha has the wrong committer identity"
    git verify-commit "$sha" >/dev/null 2>&1 ||
        die "candidate $sha lacks a trusted GhostFrame signature"
}

# Verify that GitHub attributes the remotely visible exact commit to Ghost-Frame.
verify_actor() {
    attempt=1
    while [ "$attempt" -le 10 ]; do
        if actor=$(gh api "repos/$REPOSITORY/commits/$1" --jq '.author.login // empty' 2>/dev/null); then
            [ "$actor" = Ghost-Frame ]
            return
        fi
        sleep 1
        attempt=$((attempt + 1))
    done
    return 1
}

# Remove only the isolated candidate ref after a validation failure.
discard_candidate() {
    git push origin --delete "candidate/$1" ||
        die "candidate $1 failed validation and its remote ref could not be removed"
}

# Wait until each named check reaches a successful terminal conclusion.
wait_for_checks() {
    sha=$1
    attempt=1
    while [ "$attempt" -le "$CHECK_ATTEMPTS" ]; do
        if ! checks=$(gh api "repos/$REPOSITORY/commits/$sha/check-runs?per_page=100" --jq '.check_runs[] | [.name, .status, .conclusion] | @tsv'); then
            checks=
        fi
        complete=1
        for required in \
            'Rust quality' \
            'Desktop quality' \
            'Cognition quality' \
            'Dependency audit' \
            'Secret scan' \
            'Release package contract' \
            'Windows release package contract'
        do
            line=$(printf '%s\n' "$checks" | awk -F '\t' -v check="$required" '$1 == check { print; exit }')
            if [ -z "$line" ]; then
                complete=0
                continue
            fi
            status=$(printf '%s' "$line" | awk -F '\t' '{print $2}')
            conclusion=$(printf '%s' "$line" | awk -F '\t' '{print $3}')
            if [ "$status" = completed ]; then
                [ "$conclusion" = success ] || return 1
            else
                complete=0
            fi
        done
        [ "$complete" -eq 1 ] && return 0
        [ "$attempt" -eq "$CHECK_ATTEMPTS" ] || sleep "$CHECK_INTERVAL_SECONDS"
        attempt=$((attempt + 1))
    done
    return 1
}

# Push a candidate ref, wait for checks, and fast-forward main to the unchanged SHA.
main() {
    [ "$#" -eq 1 ] || die "usage: $0 CANDIDATE_SHA"
    require_positive_integer PROMOTION_CHECK_ATTEMPTS "$CHECK_ATTEMPTS"
    require_positive_integer PROMOTION_CHECK_INTERVAL_SECONDS "$CHECK_INTERVAL_SECONDS"
    sha=$(git rev-parse --verify "$1^{commit}") || die 'candidate must resolve to a commit'
    require_gh
    verify_local_identity "$sha"
    git push origin "$sha:refs/heads/candidate/$sha"
    if ! verify_actor "$sha"; then
        discard_candidate "$sha"
        die "candidate $sha is not attributed to Ghost-Frame"
    fi
    if ! wait_for_checks "$sha"; then
        discard_candidate "$sha"
        die "required checks did not pass for $sha"
    fi
    if ! remote_main=$(git ls-remote origin refs/heads/main); then
        discard_candidate "$sha"
        die 'main reference could not be read'
    fi
    main_sha=$(printf '%s\n' "$remote_main" | awk '{print $1}')
    if [ -z "$main_sha" ]; then
        discard_candidate "$sha"
        die 'main reference is missing'
    fi
    if ! git merge-base --is-ancestor "$main_sha" "$sha"; then
        discard_candidate "$sha"
        die 'candidate is not a fast-forward of main'
    fi
    if ! git push origin "$sha:refs/heads/main"; then
        discard_candidate "$sha"
        die 'main reference could not be updated'
    fi
    git push origin --delete "candidate/$sha"
}

main "$@"
