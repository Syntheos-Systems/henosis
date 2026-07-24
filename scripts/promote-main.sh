#!/bin/sh
# Promote one exact validated candidate SHA to main through the GitHub checks API.

set -eu

PROGRAM=promote-main
REPOSITORY=${GITHUB_REPOSITORY:-Syntheos-Systems/henosis}

# Stop promotion with a precise diagnostic.
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

# Require the GitHub CLI before changing any remote references.
require_gh() { command -v gh >/dev/null 2>&1 || die 'gh CLI is required'; }

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
    for attempt in $(seq 1 10); do
        if actor=$(gh api "repos/$REPOSITORY/commits/$1" --jq '.author.login // empty' 2>/dev/null); then
            [ "$actor" = Ghost-Frame ]
            return
        fi
        sleep 1
    done
    return 1
}

# Remove only the isolated candidate ref after a remote trust failure.
discard_candidate() {
    git push origin --delete "candidate/$1" ||
        die "candidate $1 failed attribution and its remote ref could not be removed"
}

# Wait until each named check reaches a successful terminal conclusion.
wait_for_checks() {
    sha=$1
    for attempt in $(seq 1 60); do
        checks=$(gh api "repos/$REPOSITORY/commits/$sha/check-runs?per_page=100" --jq '.check_runs[] | [.name, .status, .conclusion] | @tsv')
        complete=1
        for required in \
            'Rust quality' \
            'Cognition quality' \
            'Dependency audit' \
            'Secret scan' \
            'Release package contract' \
            'Windows release package contract'
        do
            line=$(printf '%s\n' "$checks" | awk -F '\t' -v check="$required" '$1 == check { print; exit }')
            [ -n "$line" ] || complete=0
            [ "$(printf '%s' "$line" | awk -F '\t' '{print $2}')" = completed ] || complete=0
            [ "$(printf '%s' "$line" | awk -F '\t' '{print $3}')" = success ] || complete=0
        done
        [ "$complete" -eq 1 ] && return
        sleep 10
    done
    die "required checks did not pass for $sha"
}

# Push a candidate ref, wait for checks, and fast-forward main to the unchanged SHA.
main() {
    [ "$#" -eq 1 ] || die "usage: $0 CANDIDATE_SHA"
    sha=$(git rev-parse --verify "$1^{commit}") || die 'candidate must resolve to a commit'
    require_gh
    verify_local_identity "$sha"
    git push origin "$sha:refs/heads/candidate/$sha"
    if ! verify_actor "$sha"; then
        discard_candidate "$sha"
        die "candidate $sha is not attributed to Ghost-Frame"
    fi
    wait_for_checks "$sha"
    main_sha=$(git ls-remote origin refs/heads/main | awk '{print $1}')
    git merge-base --is-ancestor "$main_sha" "$sha" || die 'candidate is not a fast-forward of main'
    git push origin "$sha:refs/heads/main"
    git push origin --delete "candidate/$sha"
}

main "$@"
