# Vendored FrameShift persona crates

Mode: PRISTINE
Pin: f2a5315efb2d04f11ebdae71c09dc894e7eb22f0
Upstream: frameshift
Mirror: frameshift-source=crates/frameshift-source
Mirror: frameshift-orchestrator=crates/frameshift-orchestrator

Henosis vendors `frameshift-source` and `frameshift-orchestrator` so a checkout can build
without a sibling FrameShift repository. The two crate directories match the upstream file trees
at the pinned commit.

## Ownership

- `frameshift-source/**` and `frameshift-orchestrator/**` are pristine mirrors. Do not edit
  them in Henosis.
- `Cargo.toml` and this file belong to Henosis. The workspace manifest supplies the package and
  dependency fields inherited by the mirrored crate manifests.

## Update procedure

Set `FRAMESHIFT_UPSTREAM` to a clean FrameShift checkout, choose the reviewed commit, and replace
the two mirrored trees from that commit:

```sh
pin=<reviewed-commit>
git -C "$FRAMESHIFT_UPSTREAM" archive "$pin" \
  -- crates/frameshift-source crates/frameshift-orchestrator |
  tar -x -C vendor/frameshift --strip-components=1
```

Update the pin above, compare both trees with the upstream commit, run
`./scripts/vendor-drift.sh`, and complete the repository test suite before committing.
