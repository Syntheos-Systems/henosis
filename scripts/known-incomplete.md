# Public limitations

| Limitation | Status |
| --- | --- |
| Local policy is single-operator, loopback-only, and resets counters on restart. | Open |
| Local credential operations need a separately running loopback `phylaxd`; the quick initializer does not manufacture broker credentials. | Open |
| The embedded compatibility store's allowlisted exec mode is POSIX-only; non-Unix platforms deny it before loading secret material. | Open |
| The full proprietary Pistis service is not distributed here. Production starts with an empty room-state source, while loopback local mode authorizes only the signed `henosis.probe` compatibility action. | Open |
| The Wasmtime component host is implemented but third-party extension loading is not yet attached to the production dispatcher. | Open |
| Component compilation is admission-bounded and requires a trusted signature, but it is outside the signed execution timeout. In-process mediator implementations must enforce supplied deadlines and allocation ceilings; hard cancellation requires future process isolation. | Open |
| Indeterminate executions fail closed and cannot be retried automatically; the alpha has no operator resolution command for them. | Open |
| `henosis update` and `henosis uninstall` are reserved CLI commands and are not implemented in the alpha. | Open |
| The optional cognition facade is not part of the default build and remains incomplete. | Open |
| Production requires a separately deployed proprietary `phylaxd` broker. | Open |
| The direct `claude-max` provider hands its OAuth token to the `claude` CLI through that CLI's own environment variable, so the token is readable from `/proc/<pid>/environ` by any process sharing the UID while the subprocess runs. The multi-agent Rift bridge rejects this provider unless process isolation is implemented; direct single-agent users remain responsible for UID or PID-namespace isolation. | Open |

Machine and operator authentication, live membership checks, durable request-bound approvals, synchronous hash-chained audit, independent witness receipts, and the bounded Wasmtime host are implemented in the alpha.
