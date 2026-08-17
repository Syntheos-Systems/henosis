#!/usr/bin/env bash
# Exercise the repository rename boundary against isolated authored-source fixtures.

set -euo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SCANNER="$REPOSITORY_ROOT/scripts/rename-boundary.sh"
POLICY="$REPOSITORY_ROOT/config/product-identity.toml"
TEST_ROOT="$(mktemp -d)"

# Remove only the private fixture directory created by this test.
cleanup() {
  chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT

# Stop the regression suite with one actionable diagnostic.
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

# Run the scanner against one isolated fixture repository.
run_scanner() {
  local root="$1"
  local mode="$2"
  RENAME_BOUNDARY_ROOT="$root" \
    RENAME_BOUNDARY_POLICY="$root/config/product-identity.toml" \
    RENAME_BOUNDARY_BASELINE="$root/scripts/rename-boundary-baseline.txt" \
    "$SCANNER" "$mode"
}

# Create one fixture with a valid policy and one existing brand-coupled token.
make_fixture() {
  local root="$1"
  mkdir -p "$root/config" "$root/scripts" "$root/src"
  cp "$POLICY" "$root/config/product-identity.toml"
  printf '%s\n' 'pub const LEGACY_ID: &str = "henosis-legacy";' >"$root/src/identity.rs"
  run_scanner "$root" --write-baseline >/dev/null
}

# Assert that a scanner invocation rejects the fixture.
expect_failure() {
  local root="$1"
  local description="$2"
  if run_scanner "$root" --check >/dev/null 2>&1; then
    fail "$description unexpectedly passed"
  fi
}

[ -x "$SCANNER" ] || fail "missing executable scanner: $SCANNER"
[ -f "$POLICY" ] || fail "missing product identity policy: $POLICY"

baseline_fixture="$TEST_ROOT/baseline"
make_fixture "$baseline_fixture"
run_scanner "$baseline_fixture" --check >/dev/null

# Human-facing copy and stable machine identifiers are intentionally outside the debt ledger.
printf '%s\n' 'pub const DISPLAY_NAME: &str = "Henosis";' >>"$baseline_fixture/src/identity.rs"
printf '%s\n' 'pub const STABLE_ID: &str = "syntheos.worker";' >>"$baseline_fixture/src/identity.rs"
run_scanner "$baseline_fixture" --check >/dev/null

# Moving an existing token to a different line must not churn the count-based baseline.
line_shift_fixture="$TEST_ROOT/line-shift"
make_fixture "$line_shift_fixture"
temporary_source="$line_shift_fixture/src/identity.rs.next"
printf '%s\n' '// A line inserted before the legacy identifier.' >"$temporary_source"
sed -n '1,$p' "$line_shift_fixture/src/identity.rs" >>"$temporary_source"
mv "$temporary_source" "$line_shift_fixture/src/identity.rs"
run_scanner "$line_shift_fixture" --check >/dev/null

# Missing checked artifacts must fail rather than silently accepting an empty inventory.
missing_baseline_fixture="$TEST_ROOT/missing-baseline"
make_fixture "$missing_baseline_fixture"
rm -- "$missing_baseline_fixture/scripts/rename-boundary-baseline.txt"
expect_failure "$missing_baseline_fixture" "missing debt baseline"

missing_policy_fixture="$TEST_ROOT/missing-policy"
make_fixture "$missing_policy_fixture"
rm -- "$missing_policy_fixture/config/product-identity.toml"
expect_failure "$missing_policy_fixture" "missing identity policy"

# A new brand-prefixed environment key must be rejected.
environment_fixture="$TEST_ROOT/environment"
make_fixture "$environment_fixture"
printf '%s\n' 'pub const SECRET_KEY: &str = "HENOSIS_NEW_SECRET";' >>"$environment_fixture/src/identity.rs"
expect_failure "$environment_fixture" "new HENOSIS environment key"

# Compound environment-example files are production configuration, not generated output.
environment_example_fixture="$TEST_ROOT/environment-example"
make_fixture "$environment_example_fixture"
printf '%s\n' 'HENOSIS_NEW_CONFIG=forbidden' \
  >"$environment_example_fixture/config/production.env.example"
expect_failure "$environment_example_fixture" "new environment-example key"

# A new lower-case machine identifier must be rejected.
machine_fixture="$TEST_ROOT/machine"
make_fixture "$machine_fixture"
printf '%s\n' 'pub const WORKER_ID: &str = "henosis-new-worker";' >>"$machine_fixture/src/identity.rs"
expect_failure "$machine_fixture" "new lower-case machine identifier"

# Composite and upper-case spellings remain machine identifiers, not display copy.
case_fixture="$TEST_ROOT/case"
make_fixture "$case_fixture"
printf '%s\n' 'pub const CLIENT_ID: &str = "HenosisClient";' >>"$case_fixture/src/identity.rs"
expect_failure "$case_fixture" "new title-case composite machine identifier"

uppercase_fixture="$TEST_ROOT/uppercase"
make_fixture "$uppercase_fixture"
printf '%s\n' 'pub const BRAND_ID: &str = "HENOSIS";' >>"$uppercase_fixture/src/identity.rs"
expect_failure "$uppercase_fixture" "new upper-case machine identifier"

concatenated_fixture="$TEST_ROOT/concatenated"
make_fixture "$concatenated_fixture"
printf '%s\n' 'pub const INTERNAL_ID: &str = "myhenosisworker";' >>"$concatenated_fixture/src/identity.rs"
expect_failure "$concatenated_fixture" "new concatenated machine identifier"

# A new brand-coupled source path must fail even when its contents are display-only.
path_fixture="$TEST_ROOT/path"
make_fixture "$path_fixture"
printf '%s\n' 'pub const DISPLAY_NAME: &str = "Henosis";' >"$path_fixture/src/henosis-worker.rs"
expect_failure "$path_fixture" "new brand-coupled authored path"

# Repeating an existing token must change its count and fail the baseline check.
duplicate_fixture="$TEST_ROOT/duplicate"
make_fixture "$duplicate_fixture"
printf '%s\n' 'pub const SECOND_LEGACY_ID: &str = "henosis-legacy";' >>"$duplicate_fixture/src/identity.rs"
expect_failure "$duplicate_fixture" "duplicate legacy machine identifier"

# A policy that makes the temporary brand the technical namespace must fail closed.
policy_fixture="$TEST_ROOT/policy"
make_fixture "$policy_fixture"
sed 's/^technical_namespace = "syntheos"$/technical_namespace = "henosis"/' \
  "$policy_fixture/config/product-identity.toml" >"$policy_fixture/config/product-identity.toml.next"
mv "$policy_fixture/config/product-identity.toml.next" "$policy_fixture/config/product-identity.toml"
expect_failure "$policy_fixture" "brand-coupled technical namespace"

printf '%s\n' 'rename boundary tests passed'
