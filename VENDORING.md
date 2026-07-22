# Vendored source policy

Henosis keeps selected dependencies in the repository when a clean checkout must build without
unpublished sibling checkouts or when the workspace needs a reviewed upstream patch.

Each vendored component has a `VENDOR.md` file with four fields:

- `Mode`: `PRISTINE` or `OWNED`
- `Pin`: the reviewed upstream commit or release
- `Upstream`: the source identifier used by the drift checker
- `Ref`: the optional upstream line used for behindness checks; defaults to the checkout's `HEAD`
- `Mirror`: each local path and its upstream path, for pristine mirrors

## Pristine components

A pristine component matches the recorded upstream commit. Contributors must not edit its mirrored
files in Henosis. Update it by importing a reviewed upstream commit, changing the pin, comparing
the imported tree, and running the full repository checks.

Current pristine components:

- `vendor/kleos/kleos-config`
- `vendor/kleos/kleos-lib`
- `vendor/frameshift/frameshift-source`
- `vendor/frameshift/frameshift-orchestrator`

The workspace manifests and `VENDOR.md` files surrounding those mirrors belong to Henosis.

## Owned components

Henosis maintains owned components in this repository. Maintainers review upstream releases and
port the changes that fit Henosis. The pin records the last upstream version reviewed; it does not
promise byte-for-byte identity.

`vendor/sqlx` and `vendor/sqlx-macros-core` form one owned Postgres-only patch. Their
`vendor/sqlx/VENDOR.md` file documents the divergence and update procedure.

## Checking drift

Run:

```sh
./scripts/vendor-drift.sh
```

The checker compares pristine mirrors with their pinned commits when the corresponding upstream
checkout exists. It reports newer upstream commits that touch mirrored paths. Missing upstream
checkouts produce `SKIP` results and do not break public builds or CI.

Set `KLEOS_UPSTREAM` or `FRAMESHIFT_UPSTREAM` to override the default sibling checkout paths.
