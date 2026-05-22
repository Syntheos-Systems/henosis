# Syntheos-OS

The integration layer for the Syntheos agent stack. Syntheos-OS wires the
independent Syntheos-Systems services (Kleos, FrameShift, and the coordination
services) into one closed cognitive-behavioral loop:

```
Prescribe -> Ground -> Observe -> Evaluate -> Consolidate -> Evolve
```

Each service stays independent and is integrated over its network API, so the
loop can evolve without rewriting any single system.

## Components

- `crates/syntheos-memory-gateway` -- exposes the FrameShift memory wire
  contract and proxies it to a Kleos instance, letting FrameShift use Kleos as
  its memory backend with no changes to either system's source.

## syntheos-memory-gateway

A small HTTP service that translates between FrameShift's `frameshift-memory-http`
contract and Kleos's memory REST API.

Authentication uses Kleos's KLEOSv1 Ed25519 envelope protocol: every upstream
request is signed with an Ed25519 key whose public half is enrolled in Kleos.
There is no bearer token.

Configuration (environment only):

| Variable | Default | Purpose |
|----------|---------|---------|
| `SYNTHEOS_GATEWAY_ADDR` | `127.0.0.1:4510` | Address the gateway binds to |
| `KLEOS_BASE_URL` | `http://127.0.0.1:4200` | Upstream Kleos base URL |
| `SYNTHEOS_SIGNING_KEY_STDIN` | (unset) | When set, read the signing key from stdin (preferred -- never in env or on disk) |
| `SYNTHEOS_SIGNING_KEY_FILE` | (unset) | Path to a signing key file (raw 32 bytes, 64-char hex, or PEM PKCS8) |
| `KLEOS_IDENTITY_KEY` | (unset) | 64-char hex signing key inline (insecure: visible in `/proc/PID/environ`) |
| `SYNTHEOS_HOST_LABEL` | system hostname | Host label in the identity hash |
| `SYNTHEOS_AGENT_LABEL` | `syntheos-gateway` | Agent label in the identity hash |
| `SYNTHEOS_MODEL_LABEL` | `none` | Model label in the identity hash |

Signing-key resolution order: stdin (if `SYNTHEOS_SIGNING_KEY_STDIN` is set),
then `KLEOS_IDENTITY_KEY`, then `SYNTHEOS_SIGNING_KEY_FILE`, then
`~/.kleos/identity.key`. If no key is found the gateway starts unauthenticated
and Kleos will reject its requests.

Run (recommended -- key piped from a secret manager, never on disk or in env):

```sh
SYNTHEOS_SIGNING_KEY_STDIN=1 \
  cred exec <service> <key> --stdin -- syntheos-memory-gateway
```

Or for local development with a key file:

```sh
SYNTHEOS_SIGNING_KEY_FILE=~/.kleos/identity.key \
  cargo run -p syntheos-memory-gateway
```

### Security notes

- The gateway binds to `127.0.0.1` by default. Binding to a non-loopback
  address is logged as a warning and adds no authentication of its own -- the
  Frameshift-facing endpoints are unauthenticated, so the trust boundary is the
  host.
- Phase 1 is single-user, single-host: all clients share one Kleos identity.
  There is no per-client isolation -- any client that can reach the gateway can
  operate on any memory visible to that identity.

## License

Elastic License 2.0 (ELv2). See `LICENSE`.
