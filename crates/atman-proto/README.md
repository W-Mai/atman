# atman-proto

JSON-RPC envelopes, method identities and public request/response DTOs for the atman daemon. Unix sockets and authenticated HTTP `/rpc` use the same dispatcher. `/openapi.json` contains the public schemas.

## Capability discovery

Call `capabilities` before using optional methods. Protocol, schema and individual method revisions are independent of the package version reported by `ping`.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "capabilities",
  "params": {
    "protocol_revision": 1,
    "methods": { "flow.invoke": 1, "flow.events": 1 }
  }
}
```

Omit the requirements to inspect the supported revisions. The result contains `protocol_revision`, `schema_revision`, `host_generation` and a `methods` map. Each method has a semantic `revision` and optional `request_schema`/`response_schema` component names. An omitted schema name means that no fixed public DTO is advertised for that part of the existing operation. Method support does not authorize access to a target.

The query does not open a Session, load a Flow or start execution. Its generation changes when the daemon restarts; durable journal cursors retain their own identity. Unknown requirements fail with JSON-RPC code `-32001` and typed `CapabilityMismatch` data. Invalid query parameters return `-32602`. A daemon without this method returns `-32601`; a client can then use only the legacy contracts it explicitly supports.

## Durable responses

The `responses` module defines the existing reply shapes for Session create/open/select, Flow launch/bind/invoke, single Flow controls, Session listing, ping, run cancellation and prompt resolution. A control receipt confirms the committed operation; input acceptance confirms admission. Neither means execution has completed.

`FlowCursor` retains `{journal_id, sequence, digest}`. `flow.events` and `/events` carry durable positions. Transient Quick Ask transport, live preview, history branches and Session fork are not advertised by this protocol. `get_events` remains a historical string constant and is not a dispatched method.

`session.select_flow` persists the Session's default Flow. It is not a client-local focus command. `flow.invoke` submits an explicit invocation; it does not perform the TUI's human-text routing. `flow.control_status` queries batch controls, and `rename_session` addresses legacy Sessions. These scopes remain unchanged.
