# Vendored `sqlx` facade (Postgres-only)

Mode: OWNED
Pin: 0.8.6
Upstream: crates.io:sqlx
Content-SHA256: fbcfbeb220d0e970c8e126158900f015faeec81d8f1fca40408bccef65181792

This is a vendored, minimally-patched copy of the **sqlx 0.8.6 facade crate** wired into the
workspace via `[patch.crates-io]` in the root `Cargo.toml`.

## Why this exists

henosis pins `rusqlite = "0.31"` across the whole workspace -- including the pristine vendored
`kleos-lib` -- which links `libsqlite3-sys 0.28`. The upstream `sqlx` facade declares
`sqlx-sqlite` as an **optional** dependency. Cargo records optional dependencies in `Cargo.lock`
and enforces the `links = "sqlite3"` uniqueness constraint **even when the `sqlite` feature is
off**. Because `sqlx-sqlite >= 0.8.1` requires `libsqlite3-sys 0.30`, it collides with
rusqlite's `0.28`, and the resolver refuses to advance sqlx past `0.8.0` -- pinning the Postgres
stack to the vulnerable `rustls 0.21` / `rustls-webpki 0.101.7` and `sqlx-core 0.8.0`.

henosis only ever uses sqlx for **Postgres** (rift-server). It never uses sqlx's sqlite or
mysql backends. So this copy strips the `sqlx-sqlite` and `sqlx-mysql` optional dependencies and
every feature that references them. With no `sqlx-sqlite` in the graph there is no phantom
`libsqlite3-sys 0.30`, and the Postgres stack resolves freely to 0.8.6.

## What it fixes

- **GHSA-82j2-j2ch-gfr8 (HIGH)** -- rustls-webpki DoS: `rustls 0.21 -> 0.23`,
  `rustls-webpki 0.101.7 -> 0.103.13`.
- **GHSA-xgp8 / GHSA-965h (LOW x2)** -- rustls-webpki name-constraint issues (same bump).
- **GHSA-xmrp-424f-vfpx (MED)** -- SQLx binary-protocol cast misinterpretation:
  `sqlx-core/-postgres 0.8.0 -> 0.8.6`.

All without touching `rusqlite` or the vendored `kleos-lib`.

## What diverges from upstream

Only `Cargo.toml`. The `src/` tree is byte-for-byte upstream sqlx 0.8.6 -- every `sqlx_mysql`
/ `sqlx_sqlite` reference there is already `#[cfg]`-gated on the `mysql` / `_sqlite` features,
which are never enabled here, so those blocks compile out. `Cargo.toml.orig` is the upstream
manifest, kept for diffing on future re-vendors.

## Re-vendoring

When bumping sqlx: extract the new facade crate, replace `src/`, and re-apply the manifest edits
(drop the `sqlx-sqlite` / `sqlx-mysql` `[dependencies]` blocks and strip their tokens from the
`[features]` table, plus the `mysql` / `sqlite*` / `regexp` / `all-databases` feature keys).
Keep the sqlx-core/-postgres/-macros `=<version>` pins in lockstep with the facade version.
