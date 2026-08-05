# Security policy

Henosis is source-available public alpha software. Do not use it as the sole security boundary for an exposed production system.

## Reporting a vulnerability

Send a private report to [security@syntheos.dev](mailto:security@syntheos.dev). Include the affected version, a minimal reproduction, impact, and any safe mitigation. Do not post exploit details, credentials, private endpoints, or customer data in a public issue.

Use [support@syntheos.dev](mailto:support@syntheos.dev) for non-sensitive operational support.

## Deployment boundary

Local mode is loopback-only by default and is not a remote multi-user deployment. Production needs authenticated ingress, TLS termination, source-aware login rate limiting at the ingress, managed PostgreSQL, protected persistent storage, backups, monitoring, an independent audit witness, and the proprietary `phylaxd` credential broker. Operators manage credentials through `cred`; `phylaxd` brokers them, and Henosis contacts its separately deployed service over a private, authenticated endpoint. The process-local login limits bound password-verification work but are not a replacement for source-aware edge controls.

The repository does not distribute `phylaxd` or the private, proprietary Pistis service. A deployment that cannot authenticate and protect its broker endpoint is not production-ready. Production capability requests fail closed until a trusted room-state source from Pistis is integrated. Explicit loopback local mode creates one ephemeral signed compatibility room that authorizes only the bundled `henosis.probe` readiness action for the configured local tenant and principal.

Machine credentials are tenant-bound, scoped, revocable, and checked against live organization membership on every request. Production approvals require a different administrator principal from the request initiator, including deployments bound to loopback behind a reverse proxy. Local mode permits same-principal confirmation because it intentionally has one operator.

Operator access tokens expire after at most 15 minutes. Logging out revokes the durable refresh family immediately, but an already issued access token remains cryptographically valid until its signed expiry unless the operator loses live organization membership first. WebSocket authentication uses the `Sec-WebSocket-Protocol` offer `henosis.v1, henosis.auth.<access-token>` so credentials do not enter request URLs or ordinary server access logs. The server echoes only `henosis.v1`, rechecks live membership during the session, and closes the connection when the access token expires.

Every public dispatch requires an idempotency key scoped to the authenticated tenant and principal. The exact request is claimed durably before execution. A completed request replays only its stored, output-filtered result; the executor is not called again. Reusing a key with different request content fails as a conflict.

Model-supplied paths for `read`, `write`, `edit`, `ls`, `grep`, and `glob` are capability-confined to the task root. These tools reject absolute paths, parent traversal, and symlink escapes. This does not sandbox shell commands: `bash` is a separate capability and runs with the operating-system permissions of the Henosis process. Production deployments must grant that capability independently and add process or container isolation appropriate to their threat model.

Production startup requires synchronous audit witnessing regardless of listen address. Henosis obtains an off-host receipt for intent before execution and for a successful outcome before making its result replayable. If execution may have occurred but a safe completion cannot be established, the request is marked indeterminate and is never executed automatically on retry.

## Scope

Review [scripts/known-incomplete.md](scripts/known-incomplete.md) before deployment. The Wasmtime component host is present, but extension loading is not yet connected to the production dispatcher. Existing in-process adapters therefore remain inside the Henosis process boundary.

## Reviewed dependency exceptions

The CI dependency gate rejects RustSec advisories except for two narrow, reviewed conditions:

- `RUSTSEC-2023-0071` has no fixed `rsa` release. Henosis uses `jsonwebtoken` only with HMAC-SHA256 and YubiKey PIV only with ECDSA P-256, so the affected RSA private-key operation is outside supported execution paths. Remove this exception when the transitive dependency is fixed or removed.
- `RUSTSEC-2026-0235` affects checked access to untrusted `rkyv` archives. Cargo locks `rust_decimal` 1.42.1's optional `rkyv` 0.7 dependency, but Henosis does not activate that feature or compile the crate. Before applying the exception, CI resolves all workspace features and targets and fails if affected `rkyv` code appears through a normal, build, or development edge. CI also fails when `rkyv` 0.7 leaves the lockfile so this exception must be removed rather than retained indefinitely.

## Release integrity

Verify the SHA-256 entry in the release `SHA256SUMS` file before installation. The installers reject absent, ambiguous, malformed, or mismatched checksums and roll back an installation when initialization fails.
