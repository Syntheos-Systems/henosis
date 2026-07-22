# Security policy

## Supported code

Henosis is under active development. Security fixes target the current `main` branch. The project
does not provide security updates for older commits or unpublished builds.

## Reporting a vulnerability

Send vulnerability reports through one of these private channels:

- [GitHub Private Vulnerability Reporting](https://github.com/Syntheos-Systems/henosis/security/advisories/new)
- `security@syntheos.dev`

Include the affected commit, impact, reproduction steps, and any proposed remediation. Remove live
credentials, personal data, and unrelated secrets before attaching logs or examples.

Do not open a public issue for a vulnerability or suspected exploit. Send non-security bugs to
`support@syntheos.dev` or open a GitHub issue that contains no sensitive data.

## Deployment boundaries

Keep the integrated server on loopback because its kernel APIs accept caller-supplied tenant and
principal IDs. If you use the insecure remote-bind override during development, put an
authenticated private boundary in front of the process.

Rift also defaults to loopback. It has no in-process authentication rate limiter, so any remote
deployment needs rate limiting at a trusted proxy. Rift attachment URLs act as bearer capabilities
and do not enforce download authorization; do not store sensitive files there. Logout and password
changes revoke refresh tokens, but issued Rift access tokens remain valid for 24 hours. Rotate
`JWT_SECRET` when you need to invalidate all issued access tokens.

## Reviewed dependency exception

The dependency audit ignores only `RUSTSEC-2023-0071`, a timing side channel in the `rsa` crate's
private-key operations for which RustSec lists no fixed release. The crate is retained transitively
by `jsonwebtoken` and YubiKey support, but Henosis uses `jsonwebtoken` only for HMAC-SHA256 and uses
YubiKey PIV only for ECDSA P-256 signing. Those supported paths do not perform RSA private-key
operations.

The CI dependency gate still fails for every other RustSec vulnerability. Remove the exception as
soon as upstream dependencies provide a fixed `rsa` release, or before adding any RSA signing or
decryption path.
