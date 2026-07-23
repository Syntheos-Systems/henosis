# Vendored Kleos configuration and cognitive core

Mode: PATCHED
Pin: 4c7206bbbc661d936c46ae05a839118e905257d4
Upstream: kleos
Ref: origin/main
Mirror: kleos-config=kleos-config
Mirror: kleos-lib=kleos-lib

This directory vendors the Kleos configuration library `kleos-config` and cognitive-core library
`kleos-lib` into Henosis as a nested Cargo workspace that is excluded from the Henosis root
workspace. The crates build against their own vendored workspace root
(`vendor/kleos/Cargo.toml`), so their dependency versions stay isolated from Henosis.

## Source

- Libraries: Kleos `kleos-config` and `kleos-lib` at `main`, commit
  `4c7206bbbc661d936c46ae05a839118e905257d4`.
- Canonical upstream: `git@github.com:Ghost-Frame/Kleos.git`.
- Import only reviewed commits from the canonical mirror's `main` branch or a release tag.

## Patch status

- The source pin above identifies the reviewed upstream baseline.
- Henosis maintains a narrow phylaxd naming patch in `kleos-lib`: the raw feature, bootstrap
  environment variables and default bind, compatibility diagnostics, and retired auto-tag
  mapping use current terminology while retaining the deployed HTTP and ECDH protocol values.
- `vendor/kleos/Cargo.toml` remains a hand-maintained trimmed workspace root. Its
  `[workspace.dependencies]` table is copied verbatim from the upstream Kleos root and must
  stay in sync on every pull.

## Layout

- `vendor/kleos/Cargo.toml` -- trimmed workspace root
  (`members = ["kleos-config", "kleos-lib"]`).
- `vendor/kleos/kleos-config/` -- shared configuration crate required by `kleos-lib`.
- `vendor/kleos/kleos-lib/` -- vendored crate with the documented Henosis patch (only tracked
  files at `main`; no `target/`, no `.git`).
- The Henosis root `Cargo.toml` carries `exclude = ["vendor/kleos"]` so Cargo does not
  auto-claim this descendant as a root-workspace member. Cargo resolves kleos-lib's
  workspace root by walking UP to the first `Cargo.toml` carrying `[workspace]`, which is
  `vendor/kleos/Cargo.toml`.

## Re-vendor procedure

Before importing a new reviewed upstream commit, preserve and review the full current Henosis
delta. The patch must contain only intentional vendor changes:

```
git diff -- vendor/kleos > /tmp/henosis-kleos-vendor.patch
git apply --stat /tmp/henosis-kleos-vendor.patch
```

Import both crates from the same reviewed commit and record the new SHA in this file and in
`vendor/kleos/Cargo.toml`:

```
git -C /path/to/Kleos archive --format=tar <commit> -- kleos-config kleos-lib | tar -xC vendor/kleos/
```

After the import, re-copy the upstream `[workspace.dependencies]` table into
`vendor/kleos/Cargo.toml` if upstream changed it. Reapply the saved patch, review every hunk,
and update the recorded SHA and patch inventory:

```
git apply --3way /tmp/henosis-kleos-vendor.patch
git diff --check
git diff -- vendor/kleos
```

Resolve any rejected hunk by reviewing the upstream change and retaining the documented
phylaxd behavior only where it remains required. Do not mark the vendor pristine after applying
the Henosis delta.

## Gate (run on every pull)

```
cargo check --manifest-path vendor/kleos/Cargo.toml --workspace
```

Must be green before the pull is accepted. `ort` is built `load-dynamic` (it dlopens
`libonnxruntime.so` only when an ONNX Session is constructed), so `cargo check`/`build` do
NOT require the ONNX shared library to be present.
