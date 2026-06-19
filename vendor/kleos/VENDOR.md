# Vendored Kleos `kleos-lib`

This directory vendors the Kleos cognitive-core library `kleos-lib` into Henosis as a
NESTED Cargo workspace that is EXCLUDED from the Henosis root workspace. It builds against
its own vendored workspace root (`vendor/kleos/Cargo.toml`), so landing it forces zero
dependency changes on any existing Henosis crate.

## Source

- Library: Kleos `kleos-lib` at `main`, commit `18e507677aa325cb2d7e164b4aa52a066b3a9a17`.
- Upstream mirror (the ref deploy uses): `git@github.com:Ghost-Frame/Kleos.git`.
- The local Kleos checkout's `origin` is the GitHub mirror, while `forgejo` points at the
  internal Forgejo remote. Always vendor/pull from the GitHub mirror's `main` or a tag,
  NEVER from a live working branch.

## Read-only rule

- `vendor/kleos/kleos-lib/**` is a PRISTINE, faithful copy of upstream and is NEVER
  hand-edited. Any change to it arrives ONLY via a re-vendor / upstream pull (below).
- The ONLY hand-maintained file under `vendor/kleos/` is `vendor/kleos/Cargo.toml` -- the
  trimmed vendored workspace root. It exists so kleos-lib's `{ workspace = true }` deps
  resolve; its `[workspace.dependencies]` table is copied VERBATIM from the upstream Kleos
  root and must stay in sync on every pull.

## Layout

- `vendor/kleos/Cargo.toml` -- trimmed workspace root (`members = ["kleos-lib"]`).
- `vendor/kleos/kleos-lib/` -- pristine vendored crate (only tracked files at `main`; no
  `target/`, no `.git`).
- The Henosis root `Cargo.toml` carries `exclude = ["vendor/kleos"]` so Cargo does not
  auto-claim this descendant as a root-workspace member. Cargo resolves kleos-lib's
  workspace root by walking UP to the first `Cargo.toml` carrying `[workspace]`, which is
  `vendor/kleos/Cargo.toml`.

## Upstream-pull procedure

Primary (git subtree). Upstream Kleos maintains a `kleos-lib-split` branch produced via
`git subtree split --prefix=kleos-lib --rejoin` on the GitHub mirror:

```
git subtree pull --prefix=vendor/kleos/kleos-lib kleos-upstream kleos-lib-split --squash
```

(where `kleos-upstream` is a remote pointing at `git@github.com:Ghost-Frame/Kleos.git`).

Fallback (archive + tar) -- re-run the landing step and record the new SHA in this file and
in `vendor/kleos/Cargo.toml`:

```
git -C /path/to/Kleos archive --format=tar main -- kleos-lib | tar -xC vendor/kleos/
```

After EITHER path, re-copy the upstream `[workspace.dependencies]` table into
`vendor/kleos/Cargo.toml` if upstream changed it, and update the recorded `main` SHA.

## Gate (run on every pull)

```
cargo check --manifest-path vendor/kleos/kleos-lib/Cargo.toml
```

Must be green before the pull is accepted. `ort` is built `load-dynamic` (it dlopens
`libonnxruntime.so` only when an ONNX Session is constructed), so `cargo check`/`build` do
NOT require the ONNX shared library to be present.
