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

## Security model

Henosis treats the operating-system account, loopback interface, configured model providers, and
installed tool adapters as trust boundaries. The dispatcher governs an action after a caller has
entered those boundaries. It does not authenticate caller-supplied kernel identities or isolate
tool code from the server process.

The production dispatcher accepts one gate order: Pistis, Plutus, Eidolon, human approval, then
credential policy. A missing, reordered, or failed authority denies the action. See the
[dispatcher](crates/syntheos-dispatch/src/dispatcher.rs) and the integrated
[gate construction](crates/syntheos-server/src/app.rs).

### Threat model

| Threat | Controls in this repository | Residual risk |
|---|---|---|
| Credential exfiltration | The current legacy credential path performs sign, verify, derive, and execution operations without returning stored credential material. The [Eidolon output filter](crates/henosis-eidolon/src/output_filter.rs) redacts configured credential-bearing field names before results reach callers. Hermes obtains external adapter credentials through `phylaxd`. | Field-name redaction is not data-loss prevention. A tool or model provider can receive data that the caller sends to it. Henosis has not completed the move from the legacy in-process credential path to `cred`, brokered by `phylaxd`. |
| Prompt injection and persona drift | The [Eidolon gate](crates/henosis-eidolon/src/gate.rs) scans the action and argument tree for configured injection patterns and checks Thymus drift signals before execution. The governed mission exercises an Eidolon denial through the full dispatcher. | Pattern matching cannot identify every semantic attack. Operators must constrain tools and review untrusted content before a model converts it into an action. |
| Confused deputy and excess authority | The [dispatcher](crates/syntheos-dispatch/src/dispatcher.rs) enforces gate order. [Plutus](crates/henosis-plutus/src/gate.rs) checks organization status, membership, role, quota, and rate limits. [Pistis](crates/henosis-pistis/src/gate.rs) checks declared capabilities against room state. | Kernel routes accept caller-asserted tenant and principal IDs. Pistis examines a capability when an invocation declares one; it does not infer every capability from tool semantics. Keep these routes on loopback. |
| Cross-tenant access | Plutus scopes policy reads to a tenant and principal. Stores accept scoped identifiers, and the [task activity route](crates/syntheos-server/src/app.rs) confirms task ownership before returning lifecycle rows. | Caller-asserted identities prevent Henosis from treating the kernel HTTP surface as a remote multi-tenant boundary. An authenticated identity layer must derive these values before remote exposure. |
| Replay and duplicate delivery | The [Stripe webhook](crates/syntheos-server/src/billing.rs) verifies an HMAC over the raw body, rejects timestamps outside its tolerance, and records event IDs so a duplicate delivery cannot apply an entitlement twice. | General `/dispatch` requests have no nonce or idempotency key. A caller inside the trusted local boundary can repeat an action. Tool implementations must make destructive operations idempotent or add their own replay control. |
| Server-side request forgery | The [Synapse web fetch tool](crates/synapse-tools/src/web.rs) accepts HTTP and HTTPS, rejects credentials and non-public IP ranges, fails on mixed DNS answers, pins the selected address, and bounds response bytes. | Henosis has no shared egress policy for every Hermes adapter or model-provider endpoint. An installed adapter runs with the server process's network access. |
| Malicious or compromised tools | Hermes exposes an explicit registry and validates each invocation against its schema. The dispatcher runs all registered tools behind the same authority chain and emits lifecycle events. | Hermes adapters execute in process without an operating-system, container, or WebAssembly sandbox. A malicious adapter can exercise the server account's file and network permissions. |
| Compromised model providers | Synapse and Hephaestus let operators choose model providers and OpenAI-compatible endpoints. Credential handling stays outside model responses when the adapter uses the brokered path. | A selected provider sees prompts and other content sent to its endpoint. Henosis cannot protect data after an operator or tool sends it to a compromised provider. |
| Approval spoofing or bypass | The [human gate](crates/henosis-rift/src/gate.rs) correlates each pending request with a random approval ID and denies a timeout or explicit rejection. | A trusted invocation builder must set `requires_approval`; Henosis does not infer the requirement from all action types. The integrated server has no authenticated endpoint that calls `RegistryApprover::resolve`, so approval-required actions time out unless another trusted in-process path resolves them. |
| Audit deletion or alteration | Dispatcher lifecycle events project into Chiasm task activity and Broca action records through the [action reactor](crates/syntheos-server/src/action_reactor.rs). Owner-scoped reads expose correlated evidence. | The event bus and service databases have no signature, hash chain, write-once storage, or off-host witness. An attacker with the server account or database access can alter or delete records. |
| Release substitution | The [release workflow](.github/workflows/ci.yml) pins third-party actions to reviewed commits, gates tags on tests and scans, publishes SHA-256 checksums, and requests GitHub Sigstore attestations for each archive. | The workflow refuses to publish while the repository plan cannot issue attestations. Verify both `SHA256SUMS` and the GitHub attestation after releases begin. |

### Security claims and evidence

Run the governed mission against a local install:

```sh
./scripts/demo-governed-mission.sh
```

The command proves one clean `henosis.probe` execution, one hostile-input denial at Eidolon, and
four task-correlated lifecycle records in both Chiasm and Broca. It does not test remote
authentication, adapter isolation, audit immutability, or semantic prompt-injection resistance.

The repository test suite covers gate failure behavior, tenant and owner scoping, output
redaction, webhook replay, SSRF classification, and release-package reproducibility. Treat each
claim as specific to the tested path and commit. Review the residual-risk column before exposing
any component outside its documented boundary.

## Reviewed dependency exception

The dependency audit ignores only `RUSTSEC-2023-0071`, a timing side channel in the `rsa` crate's
private-key operations for which RustSec lists no fixed release. The crate is retained transitively
by `jsonwebtoken` and YubiKey support, but Henosis uses `jsonwebtoken` only for HMAC-SHA256 and uses
YubiKey PIV only for ECDSA P-256 signing. Those supported paths do not perform RSA private-key
operations.

The CI dependency gate still fails for every other RustSec vulnerability. Remove the exception as
soon as upstream dependencies provide a fixed `rsa` release, or before adding any RSA signing or
decryption path.
