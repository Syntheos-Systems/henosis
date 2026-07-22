#!/usr/bin/env bash
# Check pinned vendor mirrors against available upstream checkouts.

set -uo pipefail

# Resolve the repository root from this script's location.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PARENT="$(dirname "$ROOT")"
REPORT=0
FAILURES=0
CHECKED=0
SKIPPED=0
BEHIND=0

# Print command usage.
usage() {
  echo "Usage: ./scripts/vendor-drift.sh [--report]"
}

# Read one metadata field from a VENDOR.md file.
field() {
  local name="$1"
  local file="$2"
  sed -n "s/^${name}:[[:space:]]*//p" "$file" | head -n 1
}

# Resolve a documented upstream identifier to a local checkout.
upstream_checkout() {
  case "$1" in
    frameshift) printf '%s\n' "${FRAMESHIFT_UPSTREAM:-$PARENT/frameshift}" ;;
    kleos) printf '%s\n' "${KLEOS_UPSTREAM:-$PARENT/Kleos}" ;;
    *) return 1 ;;
  esac
}

# Reject paths that could escape either reviewed tree.
valid_relative_path() {
  case "$1" in
    ""|..|/*|../*|*/../*|*/..) return 1 ;;
    *) return 0 ;;
  esac
}

# Compare one pristine mirror with its tree at the pinned upstream commit.
check_mirror() {
  local vendor_dir="$1"
  local checkout="$2"
  local pin="$3"
  local mapping="$4"
  local local_rel="${mapping%%=*}"
  local upstream_rel="${mapping#*=}"
  local temp_dir

  if [ "$local_rel" = "$mapping" ] ||
     ! valid_relative_path "$local_rel" ||
     ! valid_relative_path "$upstream_rel"; then
    echo "ERROR $vendor_dir: invalid Mirror mapping '$mapping'" >&2
    return 1
  fi

  temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/henosis-vendor-drift.XXXXXX") || return 1
  case "$temp_dir" in
    "${TMPDIR:-/tmp}"/henosis-vendor-drift.*) ;;
    *) echo "ERROR: unexpected temporary path" >&2; return 1 ;;
  esac

  if ! git -C "$checkout" archive "$pin" -- "$upstream_rel" |
       tar -x -C "$temp_dir"; then
    rm -rf -- "$temp_dir"
    return 1
  fi

  if ! diff -qr "$vendor_dir/$local_rel" "$temp_dir/$upstream_rel"; then
    rm -rf -- "$temp_dir"
    return 1
  fi

  rm -rf -- "$temp_dir"
}

# Check one component and update the run counters.
check_component() {
  local vendor_file="$1"
  local vendor_dir mode pin upstream ref checkout mapping count
  local -a upstream_paths=()
  vendor_dir="$(dirname "$vendor_file")"
  mode="$(field Mode "$vendor_file")"
  pin="$(field Pin "$vendor_file")"
  upstream="$(field Upstream "$vendor_file")"
  ref="$(field Ref "$vendor_file")"
  ref="${ref:-HEAD}"

  if [ -z "$mode" ] || [ -z "$pin" ] || [ -z "$upstream" ]; then
    echo "ERROR $vendor_file: Mode, Pin, and Upstream are required" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  case "$mode" in
    PRISTINE|OWNED) ;;
    *)
      echo "ERROR $vendor_file: unsupported Mode '$mode'" >&2
      FAILURES=$((FAILURES + 1))
      return
      ;;
  esac

  if ! checkout="$(upstream_checkout "$upstream")" || [ ! -d "$checkout/.git" ]; then
    echo "SKIP  $vendor_dir: upstream checkout '$upstream' is unavailable"
    SKIPPED=$((SKIPPED + 1))
    return
  fi

  if ! git -C "$checkout" cat-file -e "${pin}^{commit}" 2>/dev/null; then
    echo "ERROR $vendor_dir: pin '$pin' is not a commit in $checkout" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if ! git -C "$checkout" cat-file -e "${ref}^{commit}" 2>/dev/null; then
    echo "ERROR $vendor_dir: ref '$ref' is not a commit in $checkout" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if [ "$mode" = "PRISTINE" ]; then
    count=0
    while IFS= read -r mapping; do
      [ -n "$mapping" ] || continue
      count=$((count + 1))
      upstream_paths+=("${mapping#*=}")
      if ! check_mirror "$vendor_dir" "$checkout" "$pin" "$mapping"; then
        echo "DRIFT $vendor_dir: mirror '$mapping' differs from $pin" >&2
        FAILURES=$((FAILURES + 1))
      fi
    done < <(sed -n 's/^Mirror:[[:space:]]*//p' "$vendor_file")
    if [ "$count" -eq 0 ]; then
      echo "ERROR $vendor_file: PRISTINE components require a Mirror field" >&2
      FAILURES=$((FAILURES + 1))
      return
    fi
  fi

  if [ "${#upstream_paths[@]}" -gt 0 ]; then
    count="$(git -C "$checkout" rev-list --count "${pin}..${ref}" -- "${upstream_paths[@]}" 2>/dev/null || echo 0)"
  else
    count="$(git -C "$checkout" rev-list --count "${pin}..${ref}" -- 2>/dev/null || echo 0)"
  fi
  if [ "$count" -gt 0 ]; then
    echo "BEHIND $vendor_dir: $count upstream commit(s) after $pin"
    BEHIND=$((BEHIND + count))
  else
    echo "OK    $vendor_dir: $mode at $pin"
  fi
  CHECKED=$((CHECKED + 1))
}

case "${1-}" in
  "") ;;
  --report) REPORT=1 ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

while IFS= read -r vendor_file; do
  check_component "$vendor_file"
done < <(find "$ROOT/vendor" -name VENDOR.md -type f | sort)

summary="vendor drift: checked=$CHECKED skipped=$SKIPPED behind=$BEHIND failures=$FAILURES"
echo "$summary"

if [ "$REPORT" -eq 1 ]; then
  if [ -n "${HENOSIS_VENDOR_DRIFT_REPORT_CMD:-}" ]; then
    "$HENOSIS_VENDOR_DRIFT_REPORT_CMD" "$summary"
  else
    echo "REPORT SKIP: HENOSIS_VENDOR_DRIFT_REPORT_CMD is not configured"
  fi
fi

[ "$FAILURES" -eq 0 ]
