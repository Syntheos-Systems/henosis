#!/bin/sh
# Reject tracked private-control files and generated cruft from the public tree.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
inventory=${1:-}

if [ -n "$inventory" ]; then
  test -f "$inventory"
  tracked_paths=$(sed '/^[[:space:]]*$/d' "$inventory")
else
  tracked_paths=$(
    git -C "$repo_root" ls-files | while IFS= read -r tracked_path; do
      if [ -e "$repo_root/$tracked_path" ] || [ -L "$repo_root/$tracked_path" ]; then
        printf '%s\n' "$tracked_path"
      fi
    done
  )
fi

findings=$(printf '%s\n' "$tracked_paths" | awk '
  /(^|\/)(AGENTS|CLAUDE|GEMINI|GROWTH)\.md$/ ||
  /(^|\/)\.(claude|codex)(\/|$)/ ||
  /(^|\/)\.env($|\.)/ ||
  /(^|\/)migration-data(\/|$)/ ||
  /(^|\/)(credentials\.json|id_rsa[^\/]*|id_ed25519[^\/]*)$/ ||
  /\.(pem|key)$/ ||
  /(^|\/)(\.DS_Store|Thumbs\.db)$/ ||
  /(^|\/)(target|node_modules|coverage)(\/|$)/ ||
  /\.(rlib|rmeta|profraw|gcda|gcno|pdb|swp|rej)$/ {
    print
  }
')

if [ -n "$findings" ]; then
  printf '%s\n' "Public repository boundary rejected tracked paths:" >&2
  printf '%s\n' "$findings" >&2
  exit 1
fi

printf '%s\n' "Public repository boundary: clean"
