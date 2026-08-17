#!/bin/sh
# Verify the public-boundary inventory gate against clean and forbidden paths.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/syntheos-public-boundary.XXXXXX")
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM

clean_inventory="$temporary_directory/clean.txt"
forbidden_inventory="$temporary_directory/forbidden.txt"

printf '%s\n' \
  '.github/workflows/ci.yml' \
  'containers/production.env.example' \
  'src/main.rs' > "$clean_inventory"
"$repo_root/scripts/public-boundary.sh" "$clean_inventory"

printf '%s\n' \
  'src/main.rs' \
  'apps/desktop/GROWTH.md' \
  'target/debug/libprivate.rlib' > "$forbidden_inventory"
if "$repo_root/scripts/public-boundary.sh" "$forbidden_inventory" \
  > "$temporary_directory/forbidden.stdout" \
  2> "$temporary_directory/forbidden.stderr"; then
  printf '%s\n' "public boundary accepted forbidden paths" >&2
  exit 1
fi

grep -F 'apps/desktop/GROWTH.md' "$temporary_directory/forbidden.stderr" > /dev/null
grep -F 'target/debug/libprivate.rlib' "$temporary_directory/forbidden.stderr" > /dev/null

"$repo_root/scripts/public-boundary.sh"
