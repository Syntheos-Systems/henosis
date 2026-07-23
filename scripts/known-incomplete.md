# Public limitations

| Limitation | Status |
| --- | --- |
| Local policy is single-operator, loopback-only, and resets counters on restart. | Open |
| Local credential operations need a separately running loopback `phylaxd`; the quick initializer does not manufacture broker credentials. | Open |
| The full proprietary Pistis service is not distributed here, and the default empty room-state source denies capability-bearing requests. | Open |
| The Wasmtime component host is implemented but third-party extension loading is not yet attached to the production dispatcher. | Open |
| Component compilation is admission-bounded and requires a trusted signature, but it is outside the signed execution timeout. In-process mediator implementations must enforce supplied deadlines and allocation ceilings; hard cancellation requires future process isolation. | Open |
| Indeterminate executions fail closed and cannot be retried automatically; the alpha has no operator resolution command for them. | Open |
| `henosis update` and `henosis uninstall` are reserved CLI commands and are not implemented in the alpha. | Open |
| Release archives have SHA-256 checksums but no independent detached signature or transparency-log proof yet. | Open |
| The optional cognition facade is not part of the default build and remains incomplete. | Open |
| Production requires a separately deployed proprietary `phylaxd` broker. | Open |

Machine and operator authentication, live membership checks, durable request-bound approvals, synchronous hash-chained audit, independent witness receipts, and the bounded Wasmtime host are implemented in the alpha.
