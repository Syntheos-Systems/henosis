#!/usr/bin/env bash
# Run the RustSec dependency gate with the repository's reviewed exception.
set -euo pipefail

# RUSTSEC-2023-0071 has no fixed rsa release. Henosis uses jsonwebtoken only
# with HMAC-SHA256 and YubiKey PIV only with ECDSA P-256, so the affected RSA
# private-key operation is outside the supported execution paths. SECURITY.md
# records the review boundary and removal condition.
exec cargo audit --ignore RUSTSEC-2023-0071
