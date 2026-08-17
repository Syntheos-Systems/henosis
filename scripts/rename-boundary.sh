#!/usr/bin/env bash
# Freeze brand-coupled machine identifiers while allowing replaceable display copy.

set -euo pipefail

REPOSITORY_ROOT="${RENAME_BOUNDARY_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)}"
POLICY="${RENAME_BOUNDARY_POLICY:-$REPOSITORY_ROOT/config/product-identity.toml}"
BASELINE="${RENAME_BOUNDARY_BASELINE:-$REPOSITORY_ROOT/scripts/rename-boundary-baseline.txt}"
MODE="${1:---check}"

# Authored-tree exclusions shared by content and path scans.
AUTHORED_GLOBS=(
  --glob '!vendor/**'
  --glob '!target/**'
  --glob '!**/target/**'
  --glob '!**/node_modules/**'
  --glob '!**/dist/**'
  --glob '!**/gen/**'
  --glob '!**/.cache/**'
  --glob '!**/coverage/**'
  --glob '!apps/desktop/release-assets/**'
  --glob '!.git'
  --glob '!.git/**'
  --glob '!Cargo.lock'
  --glob '!pnpm-lock.yaml'
  --glob '!scripts/rename-boundary.sh'
  --glob '!tests/rename-boundary.sh'
  --glob '!scripts/rename-boundary-baseline.txt'
  --glob '!config/product-identity.toml'
  --glob '!*.md'
  --glob '!*.svg'
  --glob '!*.lock'
)

# Stop the scanner with one actionable diagnostic.
fail() {
  printf 'rename-boundary: %s\n' "$*" >&2
  exit 1
}

[ "$#" -le 1 ] || fail "usage: $0 [--check|--write-baseline]"

# Read one simple quoted string from the checked product-identity policy.
policy_string() {
  local key="$1"
  sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\"\([^\"]*\)\"[[:space:]]*$/\1/p" "$POLICY"
}

# Read one integer from the checked product-identity policy.
policy_integer() {
  local key="$1"
  sed -n "s/^[[:space:]]*${key}[[:space:]]*=[[:space:]]*\([0-9][0-9]*\)[[:space:]]*$/\1/p" "$POLICY"
}

# Require one policy value to be present exactly once.
require_single_value() {
  local key="$1"
  local value="$2"
  local count
  count=$(printf '%s\n' "$value" | sed '/^$/d' | wc -l)
  [ "$count" -eq 1 ] || fail "$POLICY must define exactly one $key"
}

# Emit authored-source matches for one category and regular expression.
scan_pattern() {
  local category="$1"
  local pattern="$2"
  local allowed_exact="${3:-}"
  local matches
  local token
  local status

  set +e
  matches=$(cd "$REPOSITORY_ROOT" && rg \
    --hidden \
    --no-heading \
    --no-line-number \
    --with-filename \
    --only-matching \
    "${AUTHORED_GLOBS[@]}" \
    -- "$pattern" . 2>&1)
  status=$?
  set -e

  if [ "$status" -gt 1 ]; then
    fail "ripgrep failed while scanning $category: $matches"
  fi
  if [ -n "$matches" ]; then
    while IFS= read -r match; do
      token="${match##*:}"
      if [ -n "$allowed_exact" ] && [ "$token" = "$allowed_exact" ]; then
        continue
      fi
      printf '%s\t%s\n' "$category" "$match"
    done <<<"$matches"
  fi
}

# Emit authored file paths whose machine identity contains one guarded brand slug.
scan_paths() {
  local category="$1"
  local pattern="$2"
  local matches
  local status

  set +e
  matches=$(cd "$REPOSITORY_ROOT" \
    && rg --files --hidden "${AUTHORED_GLOBS[@]}" \
    | rg --only-matching -- "$pattern" 2>&1)
  status=$?
  set -e

  if [ "$status" -gt 1 ]; then
    fail "ripgrep failed while scanning authored paths: $matches"
  fi
  if [ -n "$matches" ]; then
    while IFS= read -r match; do
      printf '%s\t%s\n' "$category" "$match"
    done <<<"$matches"
  fi
}

# Normalize one comma-separated slug list into validated unique lines.
brand_slugs() {
  local current_slug="$1"
  local legacy_slugs="$2"
  local raw_slug
  local slug
  local -a configured_legacy_slugs=()

  printf '%s\n' "$current_slug"
  IFS=',' read -r -a configured_legacy_slugs <<<"$legacy_slugs"
  for raw_slug in "${configured_legacy_slugs[@]}"; do
    slug=$(printf '%s' "$raw_slug" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//')
    [ -n "$slug" ] && printf '%s\n' "$slug"
  done
}

# Generate stable path, token, and count tuples for all known brand slugs.
generate_inventory() {
  local slug
  local environment_prefix
  local machine_pattern
  while IFS= read -r slug; do
    [[ "$slug" =~ ^[a-z][a-z0-9-]*$ ]] \
      || fail "brand slug must match [a-z][a-z0-9-]*: $slug"
    environment_prefix=$(printf '%s' "$slug" | tr '[:lower:]-' '[:upper:]_')
    machine_pattern="(?i)\\b[A-Za-z0-9._/-]*?${slug}[A-Za-z0-9._/-]*\\b"
    scan_pattern "environment" "\\b${environment_prefix}_[A-Z0-9_]+\\b"
    scan_pattern "machine-token" "$machine_pattern" "$display_name"
    scan_paths "path-token" "$machine_pattern"
  done < <(brand_slugs "$brand_slug" "$legacy_brand_slugs" | LC_ALL=C sort -u)
}

command -v rg >/dev/null 2>&1 || fail "rg (ripgrep) is required"
command -v diff >/dev/null 2>&1 || fail "diff is required"
[ -d "$REPOSITORY_ROOT" ] || fail "repository root does not exist: $REPOSITORY_ROOT"
[ -f "$POLICY" ] || fail "product identity policy is missing: $POLICY"

schema_version=$(policy_integer schema_version)
technical_namespace=$(policy_string technical_namespace)
stable_environment_prefix=$(policy_string environment_prefix)
reverse_dns_root=$(policy_string reverse_dns_root)
display_name=$(policy_string display_name)
brand_slug=$(policy_string brand_slug)
legacy_brand_slugs=$(policy_string legacy_brand_slugs)

require_single_value schema_version "$schema_version"
require_single_value technical_namespace "$technical_namespace"
require_single_value environment_prefix "$stable_environment_prefix"
require_single_value reverse_dns_root "$reverse_dns_root"
require_single_value display_name "$display_name"
require_single_value brand_slug "$brand_slug"
require_single_value legacy_brand_slugs "$legacy_brand_slugs"

[ "$schema_version" = "1" ] || fail "unsupported product identity schema: $schema_version"
[ "$technical_namespace" = "syntheos" ] \
  || fail "technical_namespace is permanent and must remain syntheos"
[ "$stable_environment_prefix" = "SYNTHEOS" ] \
  || fail "environment_prefix is permanent and must remain SYNTHEOS"
[ "$reverse_dns_root" = "systems.syntheos" ] \
  || fail "reverse_dns_root is permanent and must remain systems.syntheos"
[ "$brand_slug" != "$technical_namespace" ] \
  || fail "display brand slug must not become the technical namespace"

current_inventory=$(mktemp "${TMPDIR:-/tmp}/henosis-rename-boundary.XXXXXX")
# Remove only the private generated inventory created by this invocation.
cleanup() {
  rm -f -- "$current_inventory"
}
trap cleanup EXIT

generate_inventory | LC_ALL=C sort | uniq -c | sed 's/^[[:space:]]*//' >"$current_inventory"

case "$MODE" in
  --check)
    [ -f "$BASELINE" ] || fail "baseline is missing: $BASELINE"
    if ! cmp -s "$BASELINE" "$current_inventory"; then
      printf '%s\n' \
        'rename-boundary: brand-coupled machine debt changed.' \
        'Review the diff. Remove accidental coupling, or deliberately refresh the baseline with:' \
        '  ./scripts/rename-boundary.sh --write-baseline' >&2
      diff -u "$BASELINE" "$current_inventory" >&2 || true
      exit 1
    fi
    printf 'rename boundary passed: %s frozen debt tuples\n' "$(wc -l <"$current_inventory")"
    ;;
  --write-baseline)
    [ ! -L "$BASELINE" ] || fail "refusing to replace symlink baseline: $BASELINE"
    mkdir -p "$(dirname "$BASELINE")"
    baseline_next="${BASELINE}.next.$$"
    cp "$current_inventory" "$baseline_next"
    chmod 0644 "$baseline_next"
    mv "$baseline_next" "$BASELINE"
    printf 'rename boundary baseline wrote %s debt tuples to %s\n' \
      "$(wc -l <"$BASELINE")" "$BASELINE"
    ;;
  *)
    fail "usage: $0 [--check|--write-baseline]"
    ;;
esac
