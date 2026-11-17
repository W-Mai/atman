# atman-client

Typed Rust clients for an existing atman runtime host. `Client::Native` calls the original owner directly; `Client::Http` uses authenticated JSON-RPC and SSE. Neither client owns runtime shutdown or retries writes after connection failures.

The shared surface covers capability discovery, Session opening, Flow listing and status, explicit invocation, input status, individual controls and durable event observation. Requests retain their original business request ID, conversation head and target. HTTP replies are checked against their original JSON-RPC ID and source identity, and credentials stay in the Authorization header.

`NativeClient::view`, `live` and `native_events` return the original typed readers without a JSON or network round trip. `host`, `session` and `flow` are trusted in-process extensions and are not wire capabilities. Ordinary TUI rendering can continue using those handles and immutable snapshots.

`NativeClient::new` binds the terminal operator. `NativeClient::legacy_rpc` binds the standalone RPC operator. Input admission and status reconcile the two retained local namespaces inside the original owner transaction: an exact retry keeps its original sender and receipt, conflicting payloads and ambiguous origins are rejected. New requests use the authenticated binding; a missing receipt never allocates a replacement request ID. Custom trusted operators remain restricted to their own namespaces. `retained_input_status` is a compatibility alias for the same single-snapshot status lookup.

`invoke` submits an explicit Invocation to the original inbox. Human-text routing, slash commands and scheduler binding remain separate host operations. The crate does not claim full remote TUI or Web parity. `FlowObservation` is a typed Rust projection of the current `flow.get` fields, including runtime execution data; it is not a new protocol schema or a promise that private fact payloads are stable across unsupported method revisions.

`EventSubscription` preserves whole committed batches and advances its reconnect cursor only when a batch is returned. Dropping an observer does not cancel execution. Reconnect with the same Flow and its last delivered cursor; an endpoint or host-generation change requires a newly authenticated client and capability negotiation. Transient readers remain native-only and cannot be represented as durable event cursors.

HTTP response and pending-event preparation have a 16 MiB byte limit. Exceeding it reports an error without installing a partial result or resubmitting an operation. The native readers retain their existing runtime budgets. This client boundary does not replace the separate bounded conversation, artifact and interaction protocols.
