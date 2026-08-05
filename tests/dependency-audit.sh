#!/usr/bin/env bash
# Exercise the dependency gate's reviewed-exception fail-closed behavior.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fixture_root="$(mktemp -d)"
trap 'rm -rf -- "${fixture_root}"' EXIT

# Prepare an isolated repository fixture with a deterministic Cargo substitute.
prepare_fixture() {
    local name="$1"
    local root="${fixture_root}/${name}"

    mkdir -p "${root}/bin" "${root}/scripts"
    cp "${repo_root}/scripts/dependency-audit.sh" "${root}/scripts/dependency-audit.sh"
    cat >"${root}/bin/cargo" <<'SCRIPT'
#!/usr/bin/env bash
# Stand in for Cargo so policy branches can be tested without network or builds.
set -euo pipefail

printf '%s\n' "$*" >>"${FAKE_CARGO_LOG:?}"
case "${1:-}" in
    tree)
        if [[ "${FAKE_CARGO_TREE_EXIT:-0}" -ne 0 ]]; then
            exit "${FAKE_CARGO_TREE_EXIT}"
        fi
        printf '%s\n' "${FAKE_CARGO_TREE:-henosis v0.0.0}"
        ;;
    audit)
        exit "${FAKE_CARGO_AUDIT_EXIT:-0}"
        ;;
    *)
        echo "unexpected cargo command: $*" >&2
        exit 64
        ;;
esac
SCRIPT
    chmod +x "${root}/bin/cargo"
    printf '%s\n' "${root}"
}

# Write the minimal lockfile shape that keeps the reviewed exception applicable.
write_affected_lock() {
    local path="$1"

    cat >"${path}" <<'LOCK'
version = 4

[[package]]
name = "rkyv"
version = "0.7.46"
LOCK
}

# Write a lockfile proving the reviewed exception has become stale.
write_clean_lock() {
    local path="$1"

    cat >"${path}" <<'LOCK'
version = 4

[[package]]
name = "serde"
version = "1.0.228"
LOCK
}

safe_root="$(prepare_fixture safe)"
write_affected_lock "${safe_root}/Cargo.lock"
FAKE_CARGO_LOG="${safe_root}/cargo.log" \
FAKE_CARGO_TREE=$'henosis v0.0.0\nrust_decimal v1.42.1' \
PATH="${safe_root}/bin:${PATH}" \
    "${safe_root}/scripts/dependency-audit.sh"
grep -Fxq 'tree --locked --workspace --all-features --target all --edges normal,build,dev --prefix none' "${safe_root}/cargo.log"
grep -Fxq 'audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0235' "${safe_root}/cargo.log"

active_root="$(prepare_fixture active)"
write_affected_lock "${active_root}/Cargo.lock"
if FAKE_CARGO_LOG="${active_root}/cargo.log" \
    FAKE_CARGO_TREE=$'henosis v0.0.0\nrust_decimal v1.42.1\nrkyv v0.7.46' \
    PATH="${active_root}/bin:${PATH}" \
    "${active_root}/scripts/dependency-audit.sh" \
    >"${active_root}/stdout" 2>"${active_root}/stderr"; then
    echo "dependency gate accepted an active affected rkyv dependency" >&2
    exit 1
fi
grep -Fxq 'RUSTSEC-2026-0235 affects an active rkyv 0.7 dependency' "${active_root}/stderr"
if grep -q '^audit ' "${active_root}/cargo.log"; then
    echo "dependency gate audited after detecting active affected rkyv" >&2
    exit 1
fi

tree_failure_root="$(prepare_fixture tree-failure)"
write_affected_lock "${tree_failure_root}/Cargo.lock"
if FAKE_CARGO_LOG="${tree_failure_root}/cargo.log" \
    FAKE_CARGO_TREE_EXIT=17 \
    PATH="${tree_failure_root}/bin:${PATH}" \
    "${tree_failure_root}/scripts/dependency-audit.sh" \
    >"${tree_failure_root}/stdout" 2>"${tree_failure_root}/stderr"; then
    echo "dependency gate accepted a failed active-graph check" >&2
    exit 1
fi
if grep -q '^audit ' "${tree_failure_root}/cargo.log"; then
    echo "dependency gate audited after the active-graph check failed" >&2
    exit 1
fi

stale_root="$(prepare_fixture stale)"
write_clean_lock "${stale_root}/Cargo.lock"
if FAKE_CARGO_LOG="${stale_root}/cargo.log" \
    PATH="${stale_root}/bin:${PATH}" \
    "${stale_root}/scripts/dependency-audit.sh" \
    >"${stale_root}/stdout" 2>"${stale_root}/stderr"; then
    echo "dependency gate accepted a stale advisory exception" >&2
    exit 1
fi
grep -Fxq 'RUSTSEC-2026-0235 exception is stale; remove it from the dependency gate' "${stale_root}/stderr"
if [[ -e "${stale_root}/cargo.log" ]]; then
    echo "dependency gate invoked Cargo before rejecting a stale exception" >&2
    exit 1
fi

echo "dependency audit policy tests passed"
