#!/usr/bin/env bash
# Check pinned vendor mirrors against available upstream checkouts.

set -uo pipefail

# Resolve the repository root from this script's location.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PARENT="$(dirname "$ROOT")"
REPORT=0
METADATA_ONLY=0
FAILURES=0
CHECKED=0
SKIPPED=0
BEHIND=0
DISCOVERED=0

# Print command usage.
usage() {
  echo "Usage: ./scripts/vendor-drift.sh [--metadata-only] [--report]"
}

# Read one metadata field from a VENDOR.md file.
field() {
  local name="$1"
  local file="$2"
  sed -n "s/^${name}:[[:space:]]*//p" "$file" | head -n 1
}

# Hash a byte stream with an available SHA-256 implementation.
sha256_stream() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 | awk '{print $1}'
  else
    echo "ERROR: sha256sum or shasum is required" >&2
    return 1
  fi
}

# Hash one Git blob with SHA-256 instead of relying on the repository object format.
sha256_blob() {
  local object="$1"
  git -C "$ROOT" cat-file blob "$object" | sha256_stream
}

# Emit a NUL-framed mode, path, and SHA-256 record for every tracked component file.
component_content_records() {
  local vendor_dir="$1"
  local relative="${vendor_dir#"$ROOT"/}"
  local path entry remainder mode object blob_sha256
  while IFS= read -r -d '' path; do
    entry="$(git -C "$ROOT" ls-files -s -- "$path")" || return 1
    mode="${entry%% *}"
    remainder="${entry#* }"
    object="${remainder%% *}"
    blob_sha256="$(sha256_blob "$object")" || return 1
    printf '%s\0%s\0%s\0' "$mode" "$path" "$blob_sha256"
  done < <(git -C "$ROOT" ls-files -z -- "$relative" ":(exclude)$relative/VENDOR.md")
}

# Hash the indexed file modes, blobs, and paths for one component except its metadata.
component_content_digest() {
  local vendor_dir="$1"
  component_content_records "$vendor_dir" | sha256_stream
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
  local vendor_dir mode pin upstream content_sha256 actual_content_sha256 ref checkout mapping count field_name field_count local_rel upstream_rel relative
  local -a upstream_paths=()
  vendor_dir="$(dirname "$vendor_file")"
  mode="$(field Mode "$vendor_file")"
  pin="$(field Pin "$vendor_file")"
  upstream="$(field Upstream "$vendor_file")"
  content_sha256="$(field Content-SHA256 "$vendor_file")"
  ref="$(field Ref "$vendor_file")"
  ref="${ref:-HEAD}"

  for field_name in Mode Pin Upstream Content-SHA256; do
    field_count="$(grep -c "^${field_name}:[[:space:]]*" "$vendor_file")"
    if [ "$field_count" -ne 1 ]; then
      echo "ERROR $vendor_file: exactly one $field_name field is required" >&2
      FAILURES=$((FAILURES + 1))
      return
    fi
  done
  field_count="$(grep -c '^Ref:[[:space:]]*' "$vendor_file")"
  if [ "$field_count" -gt 1 ]; then
    echo "ERROR $vendor_file: at most one Ref field is allowed" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if [ -z "$mode" ] || [ -z "$pin" ] || [ -z "$upstream" ] || [ -z "$content_sha256" ]; then
    echo "ERROR $vendor_file: Mode, Pin, Upstream, and Content-SHA256 are required" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  case "$content_sha256" in
    *[!0-9a-f]*|'')
      echo "ERROR $vendor_file: Content-SHA256 must be 64 lowercase hexadecimal characters" >&2
      FAILURES=$((FAILURES + 1))
      return
      ;;
  esac
  if [ "${#content_sha256}" -ne 64 ]; then
    echo "ERROR $vendor_file: Content-SHA256 must be 64 lowercase hexadecimal characters" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  case "$mode" in
    PRISTINE|PATCHED|OWNED) ;;
    *)
      echo "ERROR $vendor_file: unsupported Mode '$mode'" >&2
      FAILURES=$((FAILURES + 1))
      return
      ;;
  esac

  relative="${vendor_dir#"$ROOT"/}"
  if [ -n "$(git -C "$ROOT" ls-files --others --exclude-standard -- "$relative")" ]; then
    echo "ERROR $vendor_file: component contains untracked files" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if ! git -C "$ROOT" diff --quiet -- "$relative" ":(exclude)$relative/VENDOR.md"; then
    echo "ERROR $vendor_file: tracked component content differs from the index" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if [ -z "$(git -C "$ROOT" ls-files -- "$relative" ":(exclude)$relative/VENDOR.md")" ]; then
    echo "ERROR $vendor_file: component has no tracked content" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if ! actual_content_sha256="$(component_content_digest "$vendor_dir")"; then
    echo "ERROR $vendor_file: could not calculate component content digest" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi
  if [ "$actual_content_sha256" != "$content_sha256" ]; then
    echo "ERROR $vendor_file: component content digest differs from Content-SHA256" >&2
    FAILURES=$((FAILURES + 1))
    return
  fi

  if [ "$mode" = "PRISTINE" ] || [ "$mode" = "PATCHED" ]; then
    case "$pin" in
      *[!0-9a-f]*|'')
        echo "ERROR $vendor_file: $mode Pin must be a full lowercase commit ID" >&2
        FAILURES=$((FAILURES + 1))
        return
        ;;
    esac
    if [ "${#pin}" -ne 40 ]; then
      echo "ERROR $vendor_file: $mode Pin must be a full 40-character commit ID" >&2
      FAILURES=$((FAILURES + 1))
      return
    fi
    if ! upstream_checkout "$upstream" >/dev/null; then
      echo "ERROR $vendor_file: unsupported tracked Upstream '$upstream'" >&2
      FAILURES=$((FAILURES + 1))
      return
    fi
    count=0
    while IFS= read -r mapping; do
      [ -n "$mapping" ] || continue
      count=$((count + 1))
      local_rel="${mapping%%=*}"
      upstream_rel="${mapping#*=}"
      if [ "$local_rel" = "$mapping" ] ||
         ! valid_relative_path "$local_rel" ||
         ! valid_relative_path "$upstream_rel" ||
         [ ! -d "$vendor_dir/$local_rel" ] ||
         [ -n "$(find "$vendor_dir/$local_rel" -type l -print -quit)" ]; then
        echo "ERROR $vendor_file: invalid Mirror mapping '$mapping'" >&2
        FAILURES=$((FAILURES + 1))
        return
      fi
      upstream_paths+=("$upstream_rel")
    done < <(sed -n 's/^Mirror:[[:space:]]*//p' "$vendor_file")
    if [ "$count" -eq 0 ]; then
      echo "ERROR $vendor_file: $mode components require a Mirror field" >&2
      FAILURES=$((FAILURES + 1))
      return
    fi
  fi

  if [ "$METADATA_ONLY" -eq 1 ]; then
    echo "META  $vendor_dir: $mode at $pin"
    CHECKED=$((CHECKED + 1))
    return
  fi

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
    while IFS= read -r mapping; do
      [ -n "$mapping" ] || continue
      if ! check_mirror "$vendor_dir" "$checkout" "$pin" "$mapping"; then
        echo "DRIFT $vendor_dir: mirror '$mapping' differs from $pin" >&2
        FAILURES=$((FAILURES + 1))
      fi
    done < <(sed -n 's/^Mirror:[[:space:]]*//p' "$vendor_file")
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

while [ "$#" -gt 0 ]; do
  case "$1" in
    --metadata-only) METADATA_ONLY=1 ;;
    --report) REPORT=1 ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
  esac
  shift
done

while IFS= read -r vendor_file; do
  DISCOVERED=$((DISCOVERED + 1))
  check_component "$vendor_file"
done < <(find "$ROOT/vendor" -name VENDOR.md -type f | sort)

if [ "$DISCOVERED" -eq 0 ]; then
  echo "ERROR $ROOT/vendor: no VENDOR.md metadata files found" >&2
  FAILURES=$((FAILURES + 1))
fi

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
