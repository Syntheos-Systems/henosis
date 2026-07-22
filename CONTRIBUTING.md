# Contributing to Henosis

Henosis accepts focused bug fixes, tests, documentation, and integrations that fit the persistent
agent runtime.

## Prerequisites

- Rust 1.88 or newer
- Rust 1.94 or newer for `henosis-cognition` and the `cognition` feature
- Cargo, OpenSSL development headers, and ripgrep
- PostgreSQL for Plutus-backed server or installer tests

Clone the repository and run the default checks:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --exclude henosis-cognition -- -D warnings
cargo test --locked --workspace --exclude henosis-cognition
./scripts/stub-scan.sh
```

`henosis-cognition` compiles the vendored Kleos machine-learning stack. Test it when your change
touches Cognition, embedded memory, vector search, or the `cognition` feature, using Rust 1.94 or
newer.

## Change requirements

- Keep changes scoped. Separate dependency updates, formatting, and behavior changes.
- Add tests for changed behavior and failure paths.
- Add a concise comment to each function, type, trait, implementation, module, and method.
- Preserve fail-closed behavior in policy, credential, approval, and execution boundaries.
- Keep credentials and private infrastructure out of commits, fixtures, logs, and issue reports.

Read [VENDORING.md](VENDORING.md) before changing files under `vendor/`. Contributors must not
hand-edit pristine mirrors.

## Pull requests

Describe the problem, the chosen behavior, and the commands you ran. CI checks formatting, Clippy,
default workspace tests, the stub scanner, and complete-history secret detection.

Contributions are licensed under the repository's [Elastic License 2.0](LICENSE).

Send security reports through [SECURITY.md](SECURITY.md). Send other bugs to
`support@syntheos.dev` or a public issue without sensitive data.
