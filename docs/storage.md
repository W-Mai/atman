# Durable fact journals

`atman_runtime::event_log::journal::Journal` commits typed batches of host facts. It is separate from the asynchronous display/event writer: an accepted commit means the entire batch has been written and synchronized. A caller publishes the resulting in-memory projection and receipt only after `commit` succeeds. Streaming previews need no durable commit per chunk.

`Journal::create(path, id)` creates a new file under an existing durable parent directory. `Journal::open(path, expected_id, apply)` holds an exclusive file lock, validates the log identity and format, and replays complete batches into a private projection. The caller must discard that projection if opening fails. These operations use blocking filesystem I/O and belong on a storage worker, outside UI or executor polling.

Each JSONL journal starts with a format/version/identity header. A batch contains a consecutive sequence number, the previous record digest, the exact serialized JSON payload, and its SHA-256 digest. Payloads are nonempty arrays. The hash chain checks integrity and ordering; it does not authenticate the caller or replace host authorization. A `CommitReceipt` identifies the journal and committed sequence/digest.

The writer serializes the complete batch before touching the file. A write or synchronization error prevents further commits through that handle. Reopening can discover a complete batch whose acknowledgement was lost; the host reconciles stable request IDs before retrying effects. A journal receipt alone does not implement request deduplication or prove that a business operation completed.

Recovery preserves an unterminated final record in an adjacent `.incomplete-<uuid>` file before removing it from the active journal. `RecoveredTail` reports the retained path, original offset, and byte count. Complete malformed records, broken digest chains, sequence gaps, unsupported schemas, or projection failures reject opening without skipping history. Recovery never executes flow code.

Existing published session event files continue to use their existing reader and writer. They are not interpreted as fact journals or rewritten by this API. An embedding host must explicitly choose the new format and bind each journal to its session or flow identity.

## Session and flow storage

`flow_store::RuntimeStore::open(data_dir)` uses the isolated `runtime-v1` namespace. A `SessionStore` owns its container journal and an exclusive execution lease. Each `FlowStore` retains that lease while owning its independent journal. Another session owner cannot open the same container while any of those handles remain alive.

```text
runtime-v1/sessions/<session-id>/
  events.jsonl
  session.json
  artifacts/
  flows/<flow-instance-id>/
    events.jsonl
    flow.json
    artifacts/
```

Container facts record session creation, instance registration, and default-view selection. Flow facts record the immutable creation request, source bundle, provenance, and lifecycle transitions. `created_by` is audit metadata, not control authority. The default flow is a view binding and has no privileged lifecycle.

Creation durably prepares a flow before registering its address in the container. An unregistered prepared directory can be reconciled by retrying the same creation request and descriptor; it cannot be opened through the session catalog. Reusing a request with different creation data fails. Incomplete creation without a committed creation fact fails explicitly and preserves its files for diagnosis.

Flow transitions validate an expected state revision, apply to a private candidate, and publish that candidate only after the journal commit succeeds. Completing an invocation preserves the instance. Reopening an active invocation records `HostLost` without executing its entry; existing pause keys remain. Closing invocations retain cleanup obligations, and interrupted instances require explicit recovery before another turn can start.

`session.json` and `flow.json` are replaceable projections with a committed source cursor. They are rebuilt from journals on opening, even when missing or malformed. `take_projection_error` reports cache-write failures without withdrawing an already committed fact. Source bodies remain in the creation fact; reading history does not require a reachable project checkout.

These storage APIs do not start schedulers, grant permissions, migrate published sessions, or switch the CLI/daemon to a different data format. Hosts authorize commands, validate entry programs, and decide when restored instances may execute.

## Durable input acceptance

`FlowStore::accept_input` persists a typed delivery request before returning `Acceptance`. User submissions retain text, images, invocation configuration, and presentation data. Interjections, flow events, watcher notifications, timers, and internal controls remain distinct payload variants with sender, recipient, correlation, cause, scope, and delivery mode. A protocol message role is not used as a provenance label.

The host authenticates the sender before calling storage. A matching sender/request ID retry returns the original acceptance receipt and current disposition, including after restart, revocation, or closure. Reusing that identity with different content fails. New inputs additionally require an open instance, consistent source/payload categories, and a successful `authorize_new` callback. Host-only delivery types cannot be accepted as user or peer-authored controls.

Paused and interrupted flows retain accepted inputs in FIFO order without resuming execution. A durable close transition cancels the pending inputs in the same replayable state change, preserving their bodies and original acceptance receipts. New requests to a closing or closed instance fail; already accepted requests remain accepted facts with a cancelled disposition. Acceptance does not mean an input has reached a model or completed business work.

## Atomic input admission

`reserve_context_input` reserves eligible `AtBoundary` messages for a running turn. `reserve_invocation_input` reserves an idle instance's inputs against a candidate turn lease without starting that turn. Both use the same inbox ownership primitive. The host chooses compatible submission configurations and prepares the actual input; storage does not infer model settings, user authority, or agent policy from message text.

`commit_input` validates reservation ownership, state revision, turn lease, target, and FIFO membership. A context commit records the exact messages and consumed envelope IDs together. An invocation commit additionally records `TurnStarted`, entry arguments, and invocation configuration in that same journal batch. An entry or model request must not execute before this commit succeeds. Context messages must belong to the admitted turn, and `NextTurn` messages cannot be consumed at an active-turn boundary.

Execution state changes revoke outstanding reservations. Releasing or dropping a receipt retains pending messages; late commits and releases cannot consume or release a newer batch. Finishing, cancelling, paused, interrupted, and closed instances cannot admit ordinary context input. A failed validation leaves the queue and journal unchanged; a failed journal write requires reopening and reconciliation.

`committed_inputs` exposes durable input records with their commit receipts. Replay restores the exact input instead of preparing it again, and committed messages do not return to the pending queue after interruption or closure. Acceptance retries retain their original receipt and report `InputCommitted` once consumed. This state does not assert model inclusion or business completion. Incomplete invocation batches restore neither a turn nor consumption; accepted inputs remain pending.

## Correlated continuation input

`register_reply_waiter` durably binds a correlation ID to a turn lease and expected authenticated sender before the corresponding request is sent. Identical registration retries return the existing record; conflicting reuse and registration after matching replies have arrived fail. Multiple independent waits can belong to one turn. Correlation IDs are not authorization credentials.

`reserve_reply_input` selects the oldest matching `AtBoundary` reply from the existing inbox while the turn is in `Waiting(Reply)` and all execution pauses are released. Ordinary context and invocation reservations exclude replies owned by live waiters. Every consumer uses the same opaque batch ownership; there is no parallel reply queue. `commit_input` with `WaitingContinuation` persists the actual continuation value, consumed message ID, and delivered waiter status in one fact. It does not create a turn, wake a Rust future, or claim that the continuation executed; the host resumes execution only after successful commit and execution admission.

Pausing retains registrations and accepted replies while revoking reservations. `cancel_reply_waiter` releases unconsumed replies to ordinary input without discarding them or releasing execution pauses. Finishing, cancelling, ending, closing, and lost turns cancel their outstanding waits as part of the same durable lifecycle transition. Restart does not reconstruct suspended Rust futures: uncommitted replies remain pending behind interruption recovery, while delivered replies retain their committed input and are not delivered twice. Reply records remain available through `reply_waiters`, including cancelled and delivered states.
