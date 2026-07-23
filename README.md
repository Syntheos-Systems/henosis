# Henosis

Henosis is a source-available persistent-agent runtime in active public-alpha development. It is distributed under the [Elastic License 2.0](LICENSE), not an open-source license.

Henosis is built around one rule: an agent action is not complete merely because a model asked for it. Every public action crosses server-owned identity, policy, approval, credential, execution, and audit boundaries. The alpha includes:

- machine and human operator authentication with live tenant membership checks;
- request-bound, one-use approvals stored durably in SQLite;
- synchronous hash-chained audit records with an independently signed witness option;
- a typed Wasmtime Component Model host with fuel, memory, time, output, and network limits;
- a credential boundary that exposes named broker operations without returning raw secrets; and
- one `henosis` executable for initialization, diagnostics, control, and serving.

## Public alpha

Native release archives contain one launch executable: `henosis`. The installers select the host
platform, require a SHA-256 match from `SHA256SUMS`, install only into the current user account,
run `henosis init --quick`, and restore the prior executable if initialization fails.

Install on Linux or macOS:

```sh
curl --proto '=https' --tlsv1.2 --fail --silent --show-error --location \
  https://raw.githubusercontent.com/Syntheos-Systems/henosis/1a9ff0730f36e9a3af537e09177d36e3be204229/install.sh \
  | sh -s -- --version v0.1.0-alpha.1
```

Install from PowerShell on Windows:

```powershell
& ([scriptblock]::Create((irm 'https://raw.githubusercontent.com/Syntheos-Systems/henosis/1a9ff0730f36e9a3af537e09177d36e3be204229/install.ps1'))) `
  -Version 'v0.1.0-alpha.1'
```

Both commands pin the reviewed installer script to its full Git commit while selecting the release
version separately, so a moved tag cannot replace code before verification. Before downloading a
binary, the installer requires GitHub to report that release as published and immutable. Set
`HENOSIS_VERSION`, `HENOSIS_RELEASE_BASE`, `HENOSIS_RELEASE_API`, or `HENOSIS_INSTALL_DIR` to
override installer defaults.

The installer creates private local configuration without asking for or generating a user password. Start the loopback service:

```sh
henosis serve
```

The first boot creates a private local owner token under `HENOSIS_HOME`. Live CLI commands read that token automatically and never print it except when a newly requested token must be shown once.

Create a scoped machine token for an agent or integration:

```sh
henosis token create first-agent
```

Every public dispatch requires a unique retry identity in `X-Henosis-Idempotency-Key`. Keys are scoped to the authenticated tenant and principal. Repeating the same key and exact request returns the stored filtered result without running the tool again. Reusing the key with different content returns a conflict.

```sh
curl --fail-with-body http://127.0.0.1:8088/api/v1/dispatch \
  -H "Authorization: Bearer $HENOSIS_AGENT_TOKEN" \
  -H "Content-Type: application/json" \
  -H "X-Henosis-Idempotency-Key: first-agent-probe-1" \
  --data '{"tool":"henosis","action":"probe","args":{}}'
```

Operations that require approval return `202 Accepted` with an approval ID. Approve that ID with `henosis approvals approve <approval-id>`, then repeat the exact dispatch with both the original idempotency key and `X-Henosis-Approval-Id`.

Operator WebSocket clients authenticate without URL credentials by offering the subprotocols `henosis.v1, henosis.auth.<access-token>`. The server negotiates only `henosis.v1`, checks live organization membership during the connection, and disconnects at the token's signed expiry.

## Local development

Local mode binds to loopback by default at `127.0.0.1:8088`. It is intended for one operator and uses the embedded local policy backend. Its quota and rate-limit counters reset when the process restarts. Do not expose local mode directly to a network.

```sh
docker compose -f containers/compose.local.yml up --build
curl http://127.0.0.1:8088/health
```

The Compose service runs the same idempotent `henosis init --quick` path as the native installer. Persistent state stays in the named `henosis-state` volume.

## Production prerequisites

Production requires a managed PostgreSQL authority, protected persistent storage, TLS termination, authenticated ingress with source-aware login rate limiting, backups, an independent audit witness, and the proprietary `phylaxd` credential broker. `phylaxd` is a separately deployed dependency and must be reachable through a private, authenticated endpoint.

The full Pistis service is proprietary and is not distributed in this repository. Henosis contains a narrow fail-closed compatibility decision core. Capability-bearing requests remain denied until a deployment supplies trusted room-state integration.

### Integration boundaries

Henosis keeps model selection and capability authorization behind interfaces. Production deployments keep credential brokerage outside those interfaces.

| Boundary | Choices | Deployment contract |
| --- | --- | --- |
| Model provider | Anthropic, Ollama, an OpenAI-compatible proxy, OpenCode Zen, OpenAI Codex, Azure OpenAI, Foundry, or Claude Max CLI | Select a provider through `ProviderConfig`. The OpenAI-compatible path supports additional services without a Henosis code change. |
| Capability authority | Standalone local policy or the feature-gated Henosis room-state adapter | Select an authority through `PistisAuthority`. Restricted tools fail closed when trusted state is absent or invalid. |
| Proprietary services | `phylaxd` and the full Pistis service | Production credentials require `phylaxd`. Deployments using managed room trust supply trusted room-state integration from the full Pistis service. Neither service ships in this repository. |

Use `containers/compose.production.yml` only after:

1. Copying `containers/production.env.example` to the ignored `containers/.env.production` file and replacing every placeholder.
2. Placing a base64-encoded 32-byte audit origin signing key and the witness public key in the ignored `containers/secrets` directory with restrictive permissions.
3. Copying `HENOSIS_IMAGE_REPOSITORY` and `HENOSIS_IMAGE_DIGEST` from the release
   `container-image.env` asset.
4. Choosing the first operator email and password in the bootstrap variables. Remove both bootstrap variables after the first successful start.

Start production Compose with the same private environment file for interpolation and service
configuration:

```sh
docker compose --env-file containers/.env.production -f containers/compose.production.yml up -d
```

The image reference always contains `@sha256:`. Docker rejects a missing or malformed digest
before starting the service. The production compose file publishes no host port.

## Release verification

Every release publishes platform archives, `SHA256SUMS`, SPDX SBOMs, and Sigstore attestation
bundles. Verify an archive before use:

```sh
grep -F '  henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl.tar.gz' SHA256SUMS \
  | sha256sum --check
```

GitHub also stores OIDC-backed provenance for each archive. The GitHub CLI verifies the artifact,
repository identity, and release workflow:

```sh
gh attestation verify henosis-0.1.0-alpha.1-x86_64-unknown-linux-musl.tar.gz \
  --repo Syntheos-Systems/henosis \
  --signer-workflow Syntheos-Systems/henosis/.github/workflows/ci.yml
```

The release workflow accepts only an annotated tag signed by the GhostFrame key recorded in the
protected `release` environment's `RELEASE_ALLOWED_SIGNERS` variable. The
`security/release-allowed-signers` file is the public audit copy, not the workflow's trust source.
The tagged commit must exist on `origin/main`. Before publication, repository release immutability
must be enabled and an active `v*` tag ruleset must restrict tag creation, updates, and deletion to
GhostFrame. The workflow compares the exact remote tag object immediately before and after
publication.

The release workflow builds Linux x86-64 and arm64, macOS Intel and Apple Silicon, and Windows x86-64 archives. It also publishes a Linux amd64/arm64 container image when GitHub package publishing is available.

## Security and support

Report vulnerabilities privately to [security@syntheos.dev](mailto:security@syntheos.dev). Send non-sensitive support requests to [support@syntheos.dev](mailto:support@syntheos.dev). See [SECURITY.md](SECURITY.md) for scope and handling guidance.

## Current limitations

The public limitations ledger is [scripts/known-incomplete.md](scripts/known-incomplete.md). Alpha deployments must review it before any network exposure.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening an issue or pull request.
