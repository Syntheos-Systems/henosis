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

Configuration (environment only):

| Variable | Default | Purpose |
|----------|---------|---------|
| `SYNTHEOS_GATEWAY_ADDR` | `127.0.0.1:4510` | Address the gateway binds to |
| `KLEOS_BASE_URL` | `http://127.0.0.1:4200` | Upstream Kleos base URL |
| `KLEOS_API_KEY` | (none) | Bearer token presented to Kleos |

Run:

```sh
cargo run -p syntheos-memory-gateway
```

## License

Elastic License 2.0 (ELv2). See `LICENSE`.
