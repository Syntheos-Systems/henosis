#!/usr/bin/env bash
# stub-scan.sh -- standing stub / half-wiring scanner for Henosis-authored code.
#
# Why this exists: incremental work can hide behind "done". This scan makes the
# not-yet-wired surface visible on every run. It does three things:
#   1. HARD scan -- code that LOOKS finished but is not (todo!/unimplemented!/
#      "not implemented" panics/NotImplemented/stub comments). Any non-allowlisted
#      hit is a FAILURE (nonzero exit), so this is usable as a build/commit gate.
#   2. SOFT scan -- TODO/FIXME/placeholder/"for now"/mock/dummy/unreachable. These
#      are reported (count + locations) but do not fail the gate.
#   3. KNOWN-INCOMPLETE ledger -- prints scripts/known-incomplete.md every run, so
#      deliberately-deferred wiring (plutus deny-stub, pending rewires, partial
#      facades) is re-surfaced every time instead of being forgotten.
#
# Scope: Henosis-authored crates only. The vendored, upstream-owned cognitive core
# (vendor/) and build artifacts (target/) are NOT scanned here -- stubs inside
# vendored kleos-lib are an upstream (Kleos repo) concern, not Henosis half-wiring.
# Test code under tests/ is excluded from the HARD scan.
#
# Usage: scripts/stub-scan.sh            # scan + report, exit nonzero on hard stubs
#        STUB_SCAN_DIRS=crates/henosis-cognition scripts/stub-scan.sh   # narrow scope
# Env: STUB_SCAN_DIRS (default "crates"), STUB_ALLOWLIST (default
#      scripts/stub-allowlist.txt), STUB_KNOWN_INCOMPLETE (default
#      scripts/known-incomplete.md).
set -uo pipefail

# Repo root is the script's parent directory's parent.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Where to scan, the allowlist of accepted hits, and the ledger to reprint.
SCAN="${STUB_SCAN_DIRS:-crates}"
ALLOW="${STUB_ALLOWLIST:-scripts/stub-allowlist.txt}"
KNOWN="${STUB_KNOWN_INCOMPLETE:-scripts/known-incomplete.md}"

# HARD markers: "looks done, is not". A non-allowlisted hit fails the gate.
HARD_PAT='\btodo!\s*\(|\bunimplemented!\s*\(|panic!\s*\(\s*"[^"]*([Nn]ot[ -][Ii]mplemented|unimplemented|placeholder|\bWIP\b)|//\s*(STUB\b|stub:|[Nn]ot[ -]?[Ii]mplemented|FIXME|XXX|HACK|WIP)|\bNotImplemented\b'

# SOFT markers: tracked, never fatal.
SOFT_PAT='\bTODO\b|placeholder|\bfor now\b|\btemporary\b|\bunreachable!\(\)|\bmock\b|\bdummy\b|\bfake\b|\bstubbed\b'

# Run ripgrep over the scan scope, excluding test directories.
rg_scan() { rg -n --type rust -g '!**/tests/**' "$1" "$SCAN" 2>/dev/null; }

# Drop lines matching any allowlist regex (comments/blank lines in the allowlist
# are ignored). With no allowlist file, nothing is filtered.
filt() {
  if [ -s "$ALLOW" ]; then
    grep -vE -f <(grep -vE '^\s*#|^\s*$' "$ALLOW") || true
  else
    cat
  fi
}

echo "== Henosis stub scan ($(git rev-parse --short HEAD 2>/dev/null || echo no-git)) =="
echo "scope: $SCAN  | allowlist: $ALLOW"
echo

# HARD findings after allowlist filtering.
hard="$(rg_scan "$HARD_PAT" | filt)"

# Production composition must never quietly fall back to deny test doubles.
# Keep this target narrow so legitimate fail-closed fixtures remain usable in
# unit tests while a regressed live binary fails the completion gate.
production_deny="$(rg -n '\b(DenyExecutor|DenyGate)\b' +  crates/syntheos-server/src/main.rs 2>/dev/null || true)"
if [ -n "$production_deny" ]; then
  hard="$(printf '%s\n%s\n' "$hard" "$production_deny" | sed '/^$/d')"
fi
hard_n="$(printf '%s' "$hard" | grep -c . || true)"

# SOFT findings (informational).
soft="$(rg_scan "$SOFT_PAT" | filt)"
soft_n="$(printf '%s' "$soft" | grep -c . || true)"

echo "--- SOFT markers (informational): $soft_n ---"
if [ "$soft_n" -gt 0 ]; then printf '%s\n' "$soft" | sed 's/^/  /'; fi
echo

echo "--- HARD stubs (gate-failing): $hard_n ---"
if [ "$hard_n" -gt 0 ]; then printf '%s\n' "$hard" | sed 's/^/  /'; fi
echo

# Always reprint the curated known-incomplete ledger so deferred wiring stays visible.
if [ -f "$KNOWN" ]; then
  echo "--- KNOWN INCOMPLETE (deliberately deferred; see $KNOWN) ---"
  sed 's/^/  /' "$KNOWN"
  echo
fi

if [ "$hard_n" -gt 0 ]; then
  echo "RESULT: FAIL -- $hard_n non-allowlisted hard stub(s). Wire them, or add a justified entry to $ALLOW + $KNOWN."
  exit 1
fi
echo "RESULT: PASS -- no non-allowlisted hard stubs in $SCAN."
