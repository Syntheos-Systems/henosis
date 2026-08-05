#!/usr/bin/env bash
# Run the RustSec dependency gate with the repository's reviewed exceptions.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
cd "${repo_root}"

# Cargo locks rust_decimal's optional rkyv 0.7 dependency even though Henosis
# does not activate that feature. Keep the temporary exception self-removing:
# once the affected package leaves the lockfile, CI must require this policy to
# be deleted instead of silently retaining a stale advisory ignore.
if ! awk '
    $0 == "[[package]]" { in_package = 0 }
    $0 == "name = \"rkyv\"" { in_package = 1 }
    in_package && $0 ~ /^version = "0\.7\./ { found = 1 }
    END { exit found ? 0 : 1 }
' Cargo.lock; then
    echo "RUSTSEC-2026-0235 exception is stale; remove it from the dependency gate" >&2
    exit 1
fi

# Resolve the broadest build graph Cargo exposes and fail closed if affected
# rkyv code becomes active through any normal, build, or development edge.
feature_tree="$(cargo tree --locked --workspace --all-features --target all --edges normal,build,dev --prefix none)"
if grep -Eq '^rkyv v0\.7\.' <<<"${feature_tree}"; then
    echo "RUSTSEC-2026-0235 affects an active rkyv 0.7 dependency" >&2
    exit 1
fi

# RUSTSEC-2023-0071 has no fixed rsa release. Henosis uses jsonwebtoken only
# with HMAC-SHA256 and YubiKey PIV only with ECDSA P-256, so the affected RSA
# private-key operation is outside the supported execution paths.
#
# RUSTSEC-2026-0235 affects checked access to untrusted rkyv archives. The
# affected crate is present only as an inactive rust_decimal lockfile edge, and
# the graph check above rejects the exception if that boundary changes.
# SECURITY.md records both review boundaries and removal conditions.
exec cargo audit \
    --ignore RUSTSEC-2023-0071 \
    --ignore RUSTSEC-2026-0235
