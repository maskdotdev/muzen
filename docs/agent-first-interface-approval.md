# Agent-First Interface Approval

The reference contract in `agent-first-interface.md` passed its final API gate
on 2026-07-15 using Claude CLI in print mode with model `fable` and effort
`high`.

- Review session: `c7b1790e-3f37-49a1-9c12-2e0856ee6603`
- Revisions reviewed: 4
- Blocking findings resolved: 17
- Final checklist: 20 of 20 approved
- Final blocking findings: 0
- Final verdict: `approved`

The gate covered the runtime domain, security and credential boundaries,
low-memory behavior, local/remote parity, runner JSON-RPC, HTTP/SSE, and the
TypeScript, Python, and Rust SDK Interfaces. Any contract change after this
approval must receive a new delta review before implementation.

## Implementation API delta

The first contract implementation slice received separate Fable/high gates:

- Rust API session: `60f19ce3-4ac2-448d-bfc4-313d69eba763`
- Rust verdict: `approved`, 0 blocking findings
- TypeScript/Python API session: `82f97449-3329-4051-8947-4322089b210f`
- TypeScript verdict: `approved`, 0 blocking findings
- Python verdict: `approved`, 0 blocking findings

The implementation gate covered public declarations, strict validation,
secret redaction, exact serde/wire mapping, required and optional fields,
terminal status types, bounded artifact streaming, and cross-SDK parity. It
does not approve the not-yet-implemented local-runner or remote HTTP Adapters.
