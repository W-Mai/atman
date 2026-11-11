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
    "methods": { "flow.invoke": 2, "flow.events": 1 }
  }
}
```

Omit the requirements to inspect the supported revisions. The result contains `protocol_revision`, `schema_revision`, `host_generation` and a `methods` map. Each method has a semantic `revision` and optional `request_schema`/`response_schema` component names. An omitted schema name means that no fixed public DTO is advertised for that part of the existing operation. Method support does not authorize access to a target.

The query does not open a Session, load a Flow or start execution. Its generation changes when the daemon restarts; durable journal cursors retain their own identity. Unknown requirements fail with JSON-RPC code `-32001` and typed `CapabilityMismatch` data. Invalid query parameters return `-32602`. A daemon without this method returns `-32601`; a client can then use only the legacy contracts it explicitly supports.

## Durable responses

The `responses` module defines the existing reply shapes for Session create/open/select, Flow launch/bind/invoke, single Flow controls, Session listing, ping, run cancellation and prompt resolution. A control receipt confirms the committed operation; input acceptance confirms admission. Neither means execution has completed.

`FlowCursor` retains `{journal_id, sequence, digest}`. `flow.events` and `/events` carry durable positions. Transient Quick Ask transport, live preview and Session fork are not advertised by this protocol. `get_events` remains a historical string constant and is not a dispatched method.

`session.select_flow` persists the Session's default Flow. It is not a client-local focus command. `flow.invoke` submits an explicit invocation; it does not perform the TUI's human-text routing. `flow.control_status` queries batch controls, and `rename_session` addresses legacy Sessions. These scopes remain unchanged.

## Input status

`flow.input_status` takes `{ "flow": { "session_id": "…", "instance_id": "…" }, "request_id": "…" }` and queries the authenticated operator's original input. `InputStatusResponse` contains the requested identities and an optional body-free observation: original message ID, acceptance receipt, current input disposition and observation receipt. A committed disposition includes its exact Flow/turn/run/epoch and input commit receipt. Consumption does not mean execution completed.

Queries use the live owner's acknowledged inbox when available and recorded history otherwise. Reading after a daemon restart requires no Session open or scheduler bind and does not recover execution or repair files. The client cannot supply a sender identity. An absent observation is a sampled result: a concurrent submission can still commit. After a lost response, retry the unchanged request with its original ID; do not allocate a new identity based on an absent query result.

## Conversation head admission

Method revision 2 of `flow.get` and `goal.get` returns `history_head: { branch, version }` from the same owner observation as execution and Goal state. The version identifies conversation selection, independently of execution revisions and journal cursors. Native durable Flows have a head even before branching.

Revision 2 of `flow.invoke` and the Goal mutation methods accepts optional `expected_head`. Capture it from the conversation the client displays and preserve it with the request ID. Invocation and Goal resume submit new input, so branching requires a matching head. Other Goal edits retain their original Goal revision checks. Unbranched Flows still accept existing requests without a head. A missing required head or changed head returns code `-32002` with typed `HistoryAdmissionError` data (`required` with `current`, or `changed` with `expected` and `current`), before accepting input. A failed request must not be automatically resubmitted with the returned head.

Retries of already accepted requests retain their original payload, head and receipt, including after a later history selection. This rule also applies to accepted legacy requests without a head. Capability requirements select an exact method revision; query supported revisions before choosing a contract. Existing requests remain callable without capability negotiation, subject to the owner's history admission rule.

## History operations

`flow.history_review` accepts a stable request ID, Flow address, displayed head and a typed Revert or Restore target. Revert names its complete source cursor and original User admission or successful completion boundary; inserted input resolves to its containing turn. Restore names a retained branch. The read-only review returns a `HistoryOperationRequest` with exact execution, active lease, Goal and queue revisions plus normalized selection metadata. Review does not stop execution or hold new turns. File contents and process effects remain unchanged.

Submit the returned request unchanged to `flow.history_begin`. Acceptance records `preparing` and holds new turn admission. `flow.history_stop` authorizes cancellation of the exact reviewed active invocation; `stopping` does not mean completion. The owner reports `ready` only after the actual completion is durable, or when no invocation needed stopping. `flow.history_apply` then commits selection and any Goal pause, returning `applied`. `flow.history_abort` releases this operation's hold and returns `aborted`; it does not undo cancellation or remove other execution pauses. These methods use `{ flow, request_id }` after begin, and accepted work survives the observing caller disconnecting.

`flow.history_status` uses the same `{ flow, request_id }` to query the authenticated operator's record. It reads an existing owner's acknowledged state or inspects recorded history without opening execution, repairing journals or advancing operation stages. It works before `session.open` after daemon restart. An absent record is an observation, not proof that a concurrent begin cannot commit. Retain the exact original request for retry; reusing its ID with changed parameters is rejected. Clients cannot supply sender identity, and public operation records contain no private sender field.
