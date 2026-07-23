# Contributing to Henosis

Henosis is source-available active public-alpha software. Focus contributions on reproducible bugs, tests, documentation, and narrowly scoped integrations.

## Before opening a change

- Do not include credentials, tokens, private URLs, production data, or generated databases.
- Preserve loopback-by-default behavior and fail-closed policy boundaries.
- Add tests for behavior and failure paths.
- Comment every declaration in source files.
- Keep public documentation truthful about alpha and production limits.

## Validation

Run the checks relevant to the paths changed. Launch-surface changes use:

```sh
./tests/install.sh
./tests/release-package.sh
./tests/container-contract.sh
```

PowerShell changes also require:

```powershell
.\tests\install.ps1
.\tests\release-package.ps1
```

Rust changes must follow the repository's Rust quality checks. Do not include unrelated formatting or generated artifacts.

## Issues and security

Use the issue forms for public bugs and feature requests. Send vulnerabilities to [security@syntheos.dev](mailto:security@syntheos.dev), never to a public issue. Send non-sensitive support requests to [support@syntheos.dev](mailto:support@syntheos.dev).

Contributions are licensed under the [Elastic License 2.0](LICENSE).
