# Public limitations

| Limitation | Status |
| --- | --- |
| Local credential operations need a separately running loopback `phylaxd`; the quick initializer does not manufacture broker credentials. | Open |
| The embedded compatibility store's allowlisted exec mode is POSIX-only; non-Unix platforms deny it before loading secret material. | Open |
| The full proprietary Pistis service is not distributed here. Production starts with an empty room-state source, while loopback local mode authorizes only the signed `henosis.probe` compatibility action. | Open |
| The Wasmtime component host is implemented but third-party extension loading is not yet attached to the production dispatcher. | Open |
| Component compilation is admission-bounded and requires a trusted signature, but it is outside the signed execution timeout. In-process mediator implementations must enforce supplied deadlines and allocation ceilings; hard cancellation requires future process isolation. | Open |
| Witnessed audit recovery is an explicit human owner or administrator action through `henosis audit recover`; automated background recovery is not implemented. Execution resolution remains unavailable until the witness boundary is healthy and the exact blocked head has a valid receipt. | Open |
| CLI self-update and quarantine uninstall are implemented on Unix only. Windows needs a detached helper because a running executable cannot safely replace or move itself in-process. Graphical distributions use their platform package lifecycle. | Open |
| Production requires a separately deployed proprietary `phylaxd` broker. | Open |
| The direct `claude-max` provider hands its OAuth token to the `claude` CLI through that CLI's own environment variable, so the token is readable from `/proc/<pid>/environ` by any process sharing the UID while the subprocess runs. The multi-agent Rift bridge rejects this provider unless process isolation is implemented; direct single-agent users remain responsible for UID or PID-namespace isolation. | Open |

Machine and operator authentication, live membership checks, durable request-bound approvals, evidence-backed human resolution of indeterminate executions, synchronous hash-chained audit, independent witness receipts, and the bounded Wasmtime host are implemented.
