# Security policy

Henosis is source-available public alpha software. Do not use it as the sole security boundary for an exposed production system.

## Reporting a vulnerability

Send a private report to [security@syntheos.dev](mailto:security@syntheos.dev). Include the affected version, a minimal reproduction, impact, and any safe mitigation. Do not post exploit details, credentials, private endpoints, or customer data in a public issue.

Use [support@syntheos.dev](mailto:support@syntheos.dev) for non-sensitive operational support.

## Deployment boundary

Local mode is loopback-only by default and is not a remote multi-user deployment. Production needs authenticated ingress, TLS termination, managed PostgreSQL, protected persistent storage, backups, monitoring, an independent audit witness, and the proprietary `phylaxd` credential broker.

The repository does not distribute `phylaxd` or the full proprietary Pistis service, which is separately published from a private repository. A deployment that cannot authenticate and protect its broker endpoint is not production-ready. Capability-bearing Pistis requests fail closed until a trusted room-state source is integrated.

Machine credentials are tenant-bound, scoped, revocable, and checked against live organization membership on every request. Network production approvals require a different administrator principal from the request initiator. Local mode permits same-principal confirmation because it intentionally has one operator.

Network production startup requires synchronous audit witnessing. If terminal audit persistence fails, Henosis marks that tenant stream ambiguous and blocks further execution.

## Scope

Review [scripts/known-incomplete.md](scripts/known-incomplete.md) before deployment. The Wasmtime component host is present, but extension loading is not yet connected to the production dispatcher. Existing in-process adapters therefore remain inside the Henosis process boundary.

## Release integrity

Verify the SHA-256 entry in the release `SHA256SUMS` file before installation. The installers reject absent, ambiguous, malformed, or mismatched checksums and roll back an installation when initialization fails.
