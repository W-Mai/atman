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

`FlowStore::publish_message` commits an exact message with its owning turn lease, producing invocation, and stable publication ID. An identical retry returns the original receipt without appending or republishing; conflicting content fails. New publication requires a live matching turn, rejects cancellation and stale epochs, and permits delivery while a paused turn finishes. Message origin and role remain separate. Final-answer recognition stays in the existing message protocol rather than a second storage-specific classifier.

`context_messages` merges committed input and published output in journal order. Reopening restores that context without invoking tools, replaying notifications, or executing the entry. Publications do not end the turn or change Goal state.

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

## Durable sends

`queue_send` records a flow-owned delivery request before contacting its recipient. New sends require execution admission, consistent sender provenance, and successful host authorization. Matching request ID retries return the original record before applying new-send gates; conflicting reuse fails. An optional reply waiter must name the same correlation, current turn, and target flow. A new waiter and its send intent are committed together; an identical pending registration may also be reused.

The transport copies a recorded request to the addressed flow's `accept_input`, then passes its authenticated `DeliveryReceipt` to the sender's `confirm_delivery`. Same-session and cross-session delivery use these same storage APIs. Recipient acceptance and sender confirmation are separate commits; no cross-journal atomicity is implied. Delivery does not wait for recipient execution, and a scheduler must not hold a session-wide lock while contacting another flow.

`pending_sends` exposes unconfirmed intents in recorded order. Lost acknowledgements remain pending across restart and closure until reconciled against the recipient's idempotent acceptance. A retry can therefore recover an existing receipt after revocation or closure without authorizing a new delivery. The host must still enforce current delivery scope for previously unaccepted requests; storage does not automatically resend or bypass authorization. `confirm_delivery` accepts reconciliation after the sender has stopped, rejects conflicting confirmations, and validates the recipient journal identity. The transport authenticates the peer and binds the acknowledgement to the full address and request; a journal digest is not a signature.

An accepted send records receiver acceptance only. It does not assert that a model read the input, that a continuation executed, or that business work completed. Sender history retains the send intent, queued receipt, and receiver acknowledgement, while receiver history retains the accepted envelope and subsequent input disposition.

## Instance execution bindings

`Executor::run_linked_with_invocation` accepts `RootInvocation.instance`, an `InstanceInvocation` containing the host-admitted turn lease, explicit authority ceiling, and cancellation token. The executor registers an instance identity in the existing permission pipeline and uses the recorded turn and run IDs. It does not derive an authority ceiling from root status. Entry capability declarations may narrow the supplied ceiling but cannot broaden it. Inline calls retain the instance address and inherited restrictions.

`Session::open_instance_context(address)` creates an isolated in-memory execution context for one instance, with its own messages, Goal controller, watcher state, and interaction services. Its session ID identifies the container; its flow address identifies the context owner. Passing this context without an explicit matching instance binding fails before execution. A mismatched context, turn, or initial run also fails. Clones of an execution binding share one consumption flag, preventing reuse of the same binding for another execution attempt.

The host compiles the recorded source bundle, commits invocation input, and constructs a binding only after durable admission. It then runs the existing executor and commits the lifecycle outcome after invocation cleanup. These APIs do not themselves schedule pending inputs or revalidate pause changes during a running invocation; those responsibilities remain with the owning host. Creating a context does not create another on-disk session or replay unknown effects.

## Instance storage worker

`flow_runtime::FlowWorker::start` takes exclusive ownership of a `FlowStore` and restores its conversation through the existing message projection. Commands are serialized per instance; blocking journal work runs on the blocking pool, and an idle worker does not occupy a thread. Accepted commands complete even when their awaiting caller is cancelled. `shutdown` drains accepted work and hands the store back without changing the recorded execution lifecycle. Hosts stop active execution before handing off storage; an unexpected loss of storage cancels bound execution and leaves recovery to reconcile the recorded lifecycle.

`InstanceInvocation::with_storage` binds the executor to the worker's exact context and validates its admitted lease. `session.push`, `final.answer`, recorded model responses, and context records commit before updating context or publishing complete-message frames. The worker performs projection before acknowledging, so losing the acknowledgement does not suppress a committed reply. The same final-answer normalization and publication path serves stored and ephemeral execution. Draft streaming remains transient.

Inline calls share the instance context. Fanout calls retain isolated invocation contexts: their publications remain in the owning instance journal with `context_run`, and recovery does not merge them into the shared conversation. Message scheduling claims use the instance address rather than serializing every flow in a session.

Worker binding covers message publication, not the complete product scheduler. Legacy watcher/child notification migration, context compaction checkpoints, and legacy `flow.spawn` migration still require host integration. The CLI and daemon retain their existing storage and dispatch until that integration is complete.

## Owned instance turns

General model calls using shared session context claim compatible `AtBoundary` inbox messages before reading that context. The storage worker reserves, commits consumption and exact messages, and updates the conversation projection as one owned command. Auxiliary extraction, explicit message overrides, context-free calls, and isolated fanout branches do not claim shared input. Inline calls use the same instance inbox.

Submission settings must match the current invocation snapshot, and image submissions remain pending when the selected model cannot accept images. `NextTurn` input and replies owned by continuation waiters stay queued. Scoped input remains pending until a host scope resolver is bound. Paused and finishing turns do not claim new input. Peer deliveries retain sender metadata and a non-user origin even though the model protocol role is user. Committing context does not certify that a model received or handled it; model failures and process recovery retain the committed context without requeuing those envelopes.

`FlowWorker::start_turn` receives an explicit `TurnRequest`: ordered accepted envelope IDs, turn/run IDs, arguments, invocation configuration, context messages, authority ceiling, and cooperative yield interval. It compiles the recorded source bundle before admission, validates the selection, and commits input consumption and turn startup together. Invalid source or stale selection leaves input pending. Actual entry execution uses the existing executor and never holds the journal owner across a model call or tool wait.

The worker starts and owns the execution task before returning a `TurnTicket`. Dropping the ticket or its waiter does not cancel the task or its terminal commit. The worker permits only one execution claim for a live lease, including when callers construct separate execution bindings. Completion is acknowledged after invocation cleanup and the durable lifecycle transition. Failure and cancellation become explicit interruptions; cancellation cannot be reported as successful completion. Unknown effects are not retried. Return values remain in the completion receipt in memory; published messages remain in the flow journal.

`subscribe_state` provides the committed lifecycle snapshot, updated before a command acknowledgement. The executor waits at suspension boundaries while independent execution pauses remain, continues cleanup during cancellation, and permits finishing work to settle. Resuming one pause key does not remove others. Host-selected `yield_interval` makes CPU-only code reach suspension boundaries without an operation limit or a language-level default budget. A closing instance cancels its active turn and remains closing until the host completes resource cleanup.

`subscribe_commits` also observes inbox delivery and other durable changes that leave lifecycle state unchanged. Subscribe first, call `borrow_and_update` before `snapshot`, and await `changed` only after finding no admissible work. This order retains arrivals between an empty scan and the wait. `snapshot` reads lifecycle, journal cursor, and pending input in one serialized command. Commit notifications coalesce and wake all subscribers; readers must recheck eligibility rather than treating a notification as an execution grant. Reads, rejected writes, and idempotent retries do not emit notifications. Reopened workers expose their restored cursor and pending input immediately.

`FlowWorker::schedule` owns one event-driven admission loop per instance. A host preparation callback chooses the ordered input IDs, invocation arguments, context, and authority from a committed snapshot; it can decline input without consuming it. The callback performs local preparation, not external I/O. Admission verifies that the snapshot cursor is still current before compiling and committing the turn. A concurrent input or control change causes a fresh scan. Idle, paused, active, or interrupted instances wait for committed changes instead of polling.

After a completed turn, the scheduler rechecks pending input and can start another turn on the same instance. Interrupted execution requires explicit recovery. Preparation or storage failures stop the scheduler and remain observable through `FlowScheduler::wait`. `stop` or dropping the scheduler handle prevents further scheduling; already queued admission commands settle, and admitted turns retain their execution and terminal commit obligations. Explicit execution controls pause or cancel a turn. A stopped scheduler releases its ownership so another host controller can attach.

Input policy, instance directory ownership, management authorization, and CLI/daemon integration remain host responsibilities. Automatic admission does not assign root privileges or infer a goal from message text.

## Host directory

`flow_runtime::host::RuntimeHost` attaches exclusively owned session catalogs. Each `SessionHandle` serializes catalog creation and default-view changes on a bounded command queue. Accepted catalog operations finish even if their waiting caller disappears. Disk work runs on the blocking pool; idle catalog owners do not occupy threads.

Repeated `open_flow` and matching `create_flow` calls return the same live `FlowWorker` and conversation context. Opening another view does not reopen its journal or interrupt execution. Creation payload conflicts fail. After explicit worker handoff, reopening still requires exclusive journal ownership and reconciles an unfinished recorded turn as `HostLost`; it does not replay execution.

The host routes both same-session and cross-session delivery by complete `FlowRef`. Known instance handles are looked up without entering the catalog command queue, so unrelated catalog writes do not serialize their input or execution. Callers authenticate senders before routing, including retries, and provide authorization for new deliveries at the receiver's commit boundary. Routing does not grant authority, start a flow, or couple the recipient to its creator's lifecycle. Accepted retries retain their original receipts.

`select_default` changes only the session's view binding. It does not change execution state or permissions. Dropping catalog handles does not cancel instance handles held elsewhere. Host policies prepare eligible input using `flow_runtime::input::context_message` to retain message provenance; projection itself does not authenticate, consume, or authorize input.

The directory does not yet replace CLI/daemon session ownership. Grant management, persistent outbound delivery, managed-flow creation, and legacy history migration require the same host integration before those entry points switch storage.
