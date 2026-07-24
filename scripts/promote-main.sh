#!/bin/sh
# Promote one exact validated candidate SHA to main through the GitHub checks API.

set -eu

PROGRAM=promote-main
REPOSITORY=${GITHUB_REPOSITORY:-Syntheos-Systems/henosis}

# Stop promotion with a precise diagnostic.
die() { printf '%s: error: %s\n' "$PROGRAM" "$*" >&2; exit 1; }

# Require the GitHub CLI before changing any remote references.
require_gh() { command -v gh >/dev/null 2>&1 || die 'gh CLI is required'; }

# Verify that the GitHub-attributed author of the exact commit is Ghost-Frame.
verify_actor() {
    actor=$(gh api "repos/$REPOSITORY/commits/$1" --jq '.author.login // empty')
    [ "$actor" = Ghost-Frame ] || die "candidate $1 is not attributed to Ghost-Frame"
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
    verify_actor "$sha"
    git push origin "$sha:refs/heads/candidate/$sha"
    wait_for_checks "$sha"
    main_sha=$(git ls-remote origin refs/heads/main | awk '{print $1}')
    git merge-base --is-ancestor "$main_sha" "$sha" || die 'candidate is not a fast-forward of main'
    git push origin "$sha:refs/heads/main"
    git push origin --delete "candidate/$sha"
}

main "$@"
