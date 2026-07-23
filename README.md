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

Native release archives contain one launch executable: `henosis`. The installers select the host platform, require a SHA-256 match from `SHA256SUMS`, install only into the current user account, run `henosis init --quick`, and restore the prior executable if initialization fails.

Unix:

```sh
curl --fail --location --proto '=https' --tlsv1.2 https://raw.githubusercontent.com/Syntheos-Systems/henosis/main/install.sh | sh
```

Windows PowerShell:

```powershell
Invoke-WebRequest https://raw.githubusercontent.com/Syntheos-Systems/henosis/main/install.ps1 -OutFile install.ps1
.\install.ps1 -Headless
```

Set `HENOSIS_VERSION` to select a release tag. The alpha reference tag is `v0.1.0-alpha.1`.

The installer creates private local configuration without asking for or generating a user password. Start the loopback service:

```sh
henosis serve
```

The first boot creates a private local owner token under `HENOSIS_HOME`. Live CLI commands read that token automatically and never print it except when a newly requested token must be shown once.

## Local development

Local mode binds to loopback by default at `127.0.0.1:8088`. It is intended for one operator and uses the embedded local policy backend. Its quota and rate-limit counters reset when the process restarts. Do not expose local mode directly to a network.

```sh
docker compose -f containers/compose.local.yml up --build
curl http://127.0.0.1:8088/health
```

The Compose service runs the same idempotent `henosis init --quick` path as the native installer. Persistent state stays in the named `henosis-state` volume.

## Production prerequisites

Production requires a managed PostgreSQL authority, protected persistent storage, TLS termination, authenticated ingress, backups, an independent audit witness, and the proprietary `phylaxd` credential broker. `phylaxd` is a separately deployed dependency and must be reachable through a private, authenticated endpoint.

Pistis is separately published from a private repository and remains proprietary. Henosis contains a narrow fail-closed compatibility decision core, not the full Pistis service. Capability-bearing requests remain denied until a deployment supplies trusted room-state integration.

Use `containers/compose.production.yml` only after:

1. Copying `containers/production.env.example` to the ignored `containers/.env.production` file and replacing every placeholder.
2. Placing a base64-encoded 32-byte audit origin signing key and the witness public key in the ignored `containers/secrets` directory with restrictive permissions.
3. Pinning `HENOSIS_IMAGE` to an immutable digest and placing the service behind an authenticated TLS reverse proxy.
4. Choosing the first operator email and password in the bootstrap variables. Remove both bootstrap variables after the first successful start.

The production compose file publishes no host port.

## Release verification

Every release publishes platform archives and `SHA256SUMS`. Verify an archive before use:

```sh
sha256sum --check SHA256SUMS
```

The release workflow builds Linux x86-64 and arm64, macOS Intel and Apple Silicon, and Windows x86-64 archives. It also publishes a Linux amd64/arm64 container image when GitHub package publishing is available.

## Security and support

Report vulnerabilities privately to [security@syntheos.dev](mailto:security@syntheos.dev). Send non-sensitive support requests to [support@syntheos.dev](mailto:support@syntheos.dev). See [SECURITY.md](SECURITY.md) for scope and handling guidance.

## Current limitations

The public limitations ledger is [scripts/known-incomplete.md](scripts/known-incomplete.md). Alpha deployments must review it before any network exposure.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening an issue or pull request.
