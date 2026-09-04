# Context strategy

Atman separates durable session history, the active message window, workflow-selected
memory, and the final provider request. Treating all four as one "context window"
hides important behavior.

## The request model

Event envelopes can carry a typed `ContextId` independent of turn and execution identifiers. A scoped `EventSink` labels its emitted events without changing sibling sinks; all sinks still share the same ordered log, subscriptions, and writer. JSONL and envelope replay preserve this metadata. Missing or null context identities represent legacy unscoped records; invalid identities fail envelope deserialization. Scope metadata alone does not select a message window or enable concurrent root execution.

Root handles use the invocation's cancellation token, including an explicitly supplied token independent of the session turn. Synchronous and asynchronous spawned flows own separate entries, message handles, and compaction locks; synchronous child cancellation propagates from the caller without cancelling the caller in reverse. Normal completion updates the entry and publishes its terminal output state once. Flow events, entry status, and task status share the execution result's cancellation classification.

Execution binds its message context once through `ToolCtx`; evaluators and individual nodes do not select the session default again. Inline flows retain the caller's context, while spawned flows select their own. `memory.recent_turns` reads the bound context's current raw history directly, including messages added after binding and history retained across checkpoints. Invocation-time trust checks remain independent of this fixed message binding.

The context also binds its canonical event sink at construction. Changing a tool's diagnostic sink does not redirect messages, records, compaction publication, attachment patches, or LLM call events. Successful and failed attempts publish one call event to the owner's journal, with an additional copy only for an independent diagnostic journal. Calls without a journal-backed owner use the diagnostic sink when available. Steering consumption publishes its captured message through the destination context under the message lock, while pending and control-only transitions continue through the shared queue. Queue subscribers still receive the same state transitions. In-memory contexts explicitly omit a journal sink.

An unscoped message view selects only unscoped history, not every context in the session log. Typed contexts select their own events and the ancestry captured at creation. A child checkpoint or attachment patch cannot rewrite its source's default window or raw history; the full event log remains available for branch replay and transcript projection.

`ContextState::fork` captures a journal-backed owner's materialized views under the message-to-log lock order. It records a source identity and published cutoff, without replaying the complete event log or persisting another copy of that history. This also works after session restoration, when the live event sink contains only newly emitted events. The derived owner has independent message and compaction locks, copies the checkpoint epoch and bounded usage/prefix observations, and does not inherit a pending manual request. Full-window and complete-tool-pair inheritance use the same selection as durable replay; raw history is preserved separately. Handle-only contexts must acquire a journal-backed view before they can fork. This API does not select the session head or enable concurrent root admission.

Usage records and measured general-call window sizes belong to the bound context. Session aggregation keeps provider/model/purpose/scope buckets and cumulative input, output, and cache usage; only a general root call belonging to the session's default context updates its model, window, and latency display. Auxiliary calls and results from another context do not replace those default-context statistics.

Compaction scheduling carries its selected `ContextState` through the background worker, including its lock, pending manual request, source snapshot, usage, epoch, and canonical journal. Overflow retry releases and reacquires that same owner's lock. Session-level compaction commits accept the target explicitly and preserve the existing summary review and budget rules; only a commit to the default context refreshes its window statistics and unscoped streaming panel. Default-context convenience entry points capture their target before spawning background work.

Every `llm.call` is assembled from four independent inputs:

```text
LlmRequest
├── system
│   ├── flow-provided system prompt
│   └── runtime session context
│       ├── persistent goal, when set
│       ├── working directory
│       ├── active plan, when set
│       └── available model aliases
├── messages
│   └── selected by `messages:`, `context:`, or `prompt:`
├── input
│   └── optional structured value supplied by the flow
└── tools
    └── schemas for the tools named by the flow
```

`schema` controls structured output; it is not conversation history. Pending runtime
injections may also be rendered into the message list immediately before dispatch.
Provider adapters serialize these components into the provider-specific wire format.
Explicit tool selectors retain their declared order, wildcard matches are sorted by
qualified name, and overlaps are removed. Tool schema object keys are recursively
canonicalized, so registry insertion order does not change the provider prefix.

Before dispatch, the runtime wraps the assembled request in a provider-neutral
`ModelContextPlan`. Its `ContextPlanId` is recorded on the corresponding `llm_call`
event and does not alter provider serialization. The identifier is the correlation
key between the compiled plan and its resulting call event. The event also separates
estimated stable-instruction, tool-definition, message, and context-record tokens.
Provider usage remains authoritative when present; missing input or output counts use
the complete plan estimate and the event identifies provider, estimated, or mixed
usage. The same event classifies general, extraction, classification, and branch
generation calls and identifies detached, root-session, and spawned-child contexts.
The Session accumulates all call usage in provider/model/purpose/scope buckets. A bound context retains its latest managed-request usage separately; bare prompts and explicit `messages:` calls do not replace it. Only a general managed call belonging to the default root context updates the active model-window reading. Helper and child costs remain visible without replacing that reading. Each new `llm_call` records `managed_context`, so live accounting, raw-log replay, and daemon projection apply the same eligibility rule. Released raw events without this field retain their existing interpretation.

Managed and explicit requests use separate bounded prefix-observation tracks within each owner and purpose. An explicit request can reuse its own observation without overwriting the managed history's prefix. Only managed requests incorporate the owner's checkpoint epoch.

Call accounting consumes the same facts as the canonical call journal, including reasoning configuration failures before a plan is created. These failures count as rejected calls with zero usage and cannot replace a valid window reading. A failed isolated prompt or explicit message list does not schedule automatic compaction of the bound history using its own model budget.

Each call also fingerprints the cacheable prompt sequence after OpenAI Chat,
Anthropic Messages, Codex Responses, or provider-neutral projection. The observation
records prompt bytes and estimated tokens, plus a conservative common-prefix lower
bound at complete tool, instruction, message, or content-block boundaries. It reports
cold starts, cache enablement or disablement, provider/model/projection changes,
stable-instruction changes, tool changes, compaction, and other message-prefix
rewrites separately. Transport JSON punctuation and output-only request fields are
excluded because they do not represent the provider's semantic prompt prefix.
Successful calls record the number of tool uses returned in the assistant message.
Each dispatched tool result records raw bytes, model-visible excerpt bytes, and
whether output budgeting changed the content before it enters message history.

Dynamic model context can be represented as an internal `ContextRecord` message
part. Records carry a stable key, monotonic revision, semantic content digest,
authority, and retention policy. OpenAI chat projects them as mid-conversation
system messages, Codex Responses uses developer input items, and Anthropic uses
explicitly framed user context. Latest-value records survive compaction and remain
hidden from the normal TUI transcript while staying present in event and checkpoint
data.
Session appends compare each key against its latest semantic digest. Unchanged
content is a no-op; changed content receives the next revision and is appended as
a new internal message. Resume derives the cursor from persisted messages instead
of resetting revision state. Clearing a previously live key appends a tombstone;
an initially absent key does not create a record.

When an `llm.call` runs with a session runtime, goal, working directory, active plan,
and model information are synchronized into `session.*` records. The current user
message enters history first; changed records append after it and remain in the same
event, compaction, checkpoint, and resume stream. Isolated prompts and explicit
message lists receive only the latest live runtime records, not conversational
history. The managed agent selects relevant rules and past confessions as independently
keyed retrieved records instead of rewriting its system prompt.

The stable system template retains the agent identity, voice, durable work contract,
and general tool-use workflow. Because this prefix is stable across calls, its length
affects cold-start input and the context window but does not invalidate later cache
prefixes. Session state, retrieved rules, confessions, and changing capabilities stay
in append-only records instead of rewriting the template. The template does not
contain a working-directory placeholder. Root calls append one workspace record from
session metadata; spawned flows append one to their local history from the effective
tool workspace. This prevents duplicate root context and literal placeholders in
child requests.

## Message selection

Context creation records an explicit inheritance policy. `Full` keeps the inherited active window; `CompleteToolPairs` removes unmatched tool uses and results using the same filter as live child-context snapshots. The policy applies only at the fork boundary: later messages are not continuously filtered, and later parent results cannot change the child's fixed prefix. Raw history, image identities, and checkpoint provenance remain intact. The creation event requires this policy; no fallback infers it from older event shapes.

`ContextState` owns the message view, mutable message handle, compaction lock, checkpoint epoch, recent usage, and prefix observations. `ToolCtx` selects either a session-bound owner or an independent context; binding one replaces the other. Ordinary execution, watched calls, correction rebuilds, and inline calls retain that selection. Spawned flows share their complete state with their flow entry instead of separately constructing message, lock, and cache handles. Session views still use the event-backed window, and raw history remains separate from compaction. Root and child checkpoint epochs use the checkpoint contents rather than a process-local child counter.

Context cache identity belongs to the owner, not each inline execution. Root calls use the session identity; child and detached calls with a flow entry use that entry's run identity. Starting another inline helper does not split usage records, reset prefix observations, or change the provider cache key. Calls without an entry retain their execution-run fallback. Execution events, authority checks, and tool exposure remain attributed to the actual calling run.

LLM responses, correction output, and internal records share the calling context's append path. Session-bound contexts persist messages and attachment patches to the session log even when execution diagnostics use a separate sink; independent contexts use their scoped sink. Appends preserve message and image identities, actual assistant run correlation, internal-record visibility, and root Mermaid output. Runtime record synchronization obtains session metadata before locking the context, and successful managed calls retain their context lock through response insertion.

Replay derives the checkpoint epoch from the last applicable checkpoint event, including an empty checkpoint. It does not infer the epoch from the remaining messages or synthetic message positions. Root replay excludes spawned checkpoints; selected context replay respects ancestry and source cutoffs. Message changes after a checkpoint do not redefine its epoch. `ReplayBundle::view.checkpoint_epoch` and `ContextReplay::checkpoint_epoch` expose this restored value.

Image parts carry an optional persistent `MessagePartId`. Context insertion assigns missing IDs; copies, part filtering, and checkpoints preserve them. Legacy replay derives missing IDs from the message turn and event coordinates, including the message index for checkpoint contents. A legacy checkpoint without IDs receives its own references rather than guessing a relationship with earlier messages. IDs remain in stored messages but are excluded from provider input, provider-neutral cache prefixes, and checkpoint epoch digests.

`projection::context::replay_context` accepts an explicit `ContextBase` and reconstructs one bounded ancestry from event envelopes. `ContextCreated` without a base starts empty; a `legacy_root` base selects unscoped root history, while a `context` base requires an existing context identity and an earlier cutoff. Descendants inherit the source's active window at that cutoff, not later parent output. Scoped checkpoints and compaction affect only the selected ancestry; raw messages remain available independently. Creation records contain references, not repeated copies of complete history. This replay API does not enable concurrent root admission.

`ContextHeadSelected` identifies the context selected for an accepted turn. `replay_default_context` resolves the last selection and reconstructs only its bounded ancestry, independently of late output or checkpoints from other branches. Without a selection it retains the legacy root view. `ContextReplay::includes` exposes the same event boundaries for related metadata. Replay accepts cloneable borrowed iterators, avoiding a second copy of the event log. Missing, unknown, or malformed context identities are errors, not instructions to fall back to unscoped history. Head selection records preserve their identifiers through log redaction. These journal APIs do not enable concurrent admission.

`select_default_context` returns a lightweight `ContextSelection` without copying message contents. Metadata-only readers use that selection to restore aggregate costs and the chosen context's model and window reading. `SessionReplay` retains the materialized view directly and validates lineage before notifying transcript observers. Session reopening binds the view and journal to its selected identity, derives the writable handle from the active window, and restores bounded managed-call usage and checkpoint state. Context-state caches carry their owner and event watermark; a cache from another context, before a newer model-window fact, or ahead of the available event log cannot override replay. Explicit helper calls do not invalidate an otherwise current cache. Missing caches leave reconstructed values intact.

`MessageStream::from_context` creates a live view from that validated ancestry while holding the shared log lock across initialization. It records the consumed log position and subsequently processes only newly appended events owned by its context. Other branches do not rebuild its cached window or raw history. `MessageStream::new` creates an unscoped view; `with_initial` requires the restored context identity alongside its compacted and raw state. Context construction verifies that its stream and sink use the same scope and journal.

Compaction commits the complete selected replacement window to both live message handles and its persisted checkpoint. Latest retained context records, including tombstones that clear a previous value, remain after the summary. Raw history remains independent of this replacement.

When retained turns also exceed the budget, their output can be summarized or replaced with an omission notice. Latest context records within that output survive the rewrite unless a later record already replaces the same key. Timeline records participate in the summary; current state and tombstones remain explicit through the minimum-window policy.

Root and child compaction records hold the shared event-log lock across publication. Log snapshots and context-view initialization therefore see the batch before or after its complete publication, while event subscribers and the writer still receive individual records. This lock does not span provider requests and does not make multiple JSONL records a disk transaction.

Message appends acquire the context message lock before the event-log lock. Root messages, explicit `session.push`, child assistant output, context records, and captured steering use this lock order. The shared append path updates its message handle before sending the live frame. A context snapshot must respect this ordering; an event watermark read outside the commit locks is not an atomic snapshot of the mutable handle.

Child compaction computes its candidate without holding the message or event-log lock. It then reacquires the message lock, compares the current history with the source snapshot, and invokes its synchronous commit callback to update the epoch and publish the checkpoint before replacing the handle. A stale candidate performs no commit and preserves concurrent additions. Normal compaction and overflow retries use the same commit boundary; the callback must not reenter the message handle.

The flow chooses one message source:

| Form | Messages sent |
|---|---|
| `messages: [...]` | The latest live runtime records followed by the explicit message list. Cannot be combined with `prompt:` or `context:`. |
| `context: "session"` | The current session message window. An optional `prompt:` is appended as a user message. |
| `context: "session_recent(n)"` | The latest live records followed by the last `n` non-record messages from the current session window. |
| `prompt: "..."` | The latest live runtime records followed by one user message, with no conversational history. This is also the default when `context:` is omitted. |

The managed `commands/agent.at` uses `context: "session"`. It does **not** feed `memory.recent_turns` to the main model as a fixed sliding window.

At the start of the managed flow, `memory.recent_turns(n: 5, excerpt_chars: 12000)` provides a bounded excerpt to the cheap rule/confession selector. The same bounded view is used later by the loop-disposition classifier. Its lossless `items` remain available to explicit callers, and these helper calls do not define the main model's session context.

Pending nudges and course corrections are reserved for general agent calls. Classification, extraction, branch generation, compaction, and interjection classification do not consume these messages, including through the streaming monitor. Streaming helper calls still accept redirect and hard-stop controls and retain their normal output routing. The managed flow decides whether to continue after checking pending input.

Control selection prioritizes hard stops, then redirects, then course corrections, preserving arrival order within each level. Run control consumption is atomic: one consumer claims a stop or redirect while nudges, corrections, and other runs remain untouched. Session submission and `flow.interject("root", ...)` share the root inbox; spawned flows have separate inboxes, and inline subflows retain their caller's inbox. Inputs accepted before root execution starts keep their IDs when bound to that execution. Terminal runs reject late input and cancel their pending messages; ending a root turn does not cancel independently running child inboxes.

An L1 nudge received during a request stays pending until the next general agent call. An L2 correction interrupts that request and returns an exclusive, cancellation-safe claim to LLM dispatch. Dispatch persists the partial assistant response and captured correction under the context's compaction lock, then rebuilds through the same context selection used for ordinary calls. Dropping an uncommitted claim makes it available again unless the target has ended. Corrections do not consume the provider retry budget or have a fixed restart limit, and monitoring does not require a UI subscriber. Explicit `prompt:` and `messages:` inputs retain their per-call semantics; a restart renders them once and carries the current call's steering suffix without persisting or duplicating the original prompt.

Session steering consumption records the rendered message and consumed state in one event under the compaction lock. The canonical context retains that message for later calls and resume without creating a transcript turn boundary. Current-call steering is included in addition to the requested historical slice for `session_recent(n)`. Bare `prompt:` and explicit `messages:` requests include newly consumed steering only for that call; they do not implicitly read conversational history. Overflow retries use the same context builder and do not append a second steering suffix. Older queue-only events without captured messages are not re-rendered during replay.

## Session history and active window

History queries include captured interjection messages in both SQLite and in-memory or JSONL reads. Message filtering occurs before SQL counting and pagination; queue-only updates do not occupy message positions. Live indexing and index rebuild use the same message text and run anchors, so consumed steering remains searchable without exposing pending or cancelled input as completed history.

Daemon projections include captured steering in their message positions before applying compaction ranges. A consumed message and its interaction state are published in one delta; pending, cancelled, and legacy queue-only records add no message position. The message retains its interjection origin rather than becoming a new user turn. Client renderers must preserve these hidden positions when applying updates. Snapshot recovery and live updates use the same event message contract. Private projection caches require the current cache version and an explicit event cursor; invalid caches are rebuilt from the original log, without migration or inferring a cursor from a revision.

The durable event stream is the source of session history. The runtime derives the
message stream from user, assistant, and tool events, plus checkpoints. The active
window is the message list currently used by `context: "session"`.

Before compaction these are effectively the retained conversation messages. After
compaction, the active window contains a structured compact summary followed by
recent messages. A checkpoint persists that replacement and synchronizes the live
message handle, so subsequent `context: "session"` calls use the compacted window.

Spawned flows keep a separate message segment. With `inherit_context: true`, the child starts from a copy of the parent's active window and then appends its invocation message. That copy contains only complete tool-use/result pairs, so the active parent `flow.spawn` transaction and other concurrently executing tools do not become orphaned requests in the child context.

Successful managed `llm.call(context: "session")` calls append their assistant message to the active root or child segment through the same runtime contract. Flow code uses `session.push` for tool results, interjections, and other explicit messages rather than re-appending the returned assistant message.

Child workspace, parent handoff, and retrieved context records use the same versioned append operation. Each changed record is persisted with its child owner before entering memory; identical content does not create another event. Course corrections preserve these records alongside partial assistant responses and captured steering. Bare `prompt:` calls remain hidden and do not append their final response automatically.

Child segments use the same token-aware, transaction-aligned compaction and checkpoint mechanism as root history. No message-count trim rewrites the child prefix outside compaction; each child retains its own context epoch and cache-prefix observations.

Tool results use the same configured line, byte, and per-line budget in dispatch,
`session.push`, child segments, and direct Session appends. The Executor projects the
active budget into the Session at root-flow start, while spawned ToolCtx values inherit
the same snapshot.

Before provider projection, one canonical tool-pair normalizer moves every result next
to its assistant call, emits parallel results in call order, fills interrupted calls,
and removes only orphan result parts. Ordinary text in mixed messages remains visible.

Each root invocation also owns a transient request-exposure registry. Successful LLM
responses register only tool-use IDs and canonical names that appeared in that request's
tool definitions. `dispatch_all` claims the matching flow-scoped entry before consulting
the global registry; unexposed, renamed, duplicate-ID, cross-flow, and replayed calls are
returned as tool errors without execution.

History recall is separate from automatic prompt assembly:

- `memory.history.search` searches persisted session messages through FTS5.
- `memory.history.read` reads a selected range.
- `memory.history.count` reports the available history size.
- `memory.recent_turns` returns lossless recent turns and can produce a separately
  bounded excerpt for workflow logic.

Search results are not inserted into the next prompt automatically. The flow or agent
must inspect them and deliberately carry the relevant facts forward, for example in a
message, the goal, the active plan, or a tool result.

## Budget and compaction

Root and child compaction commit against the exact source messages used to build the candidate. The shared context commit checks the source under the message lock before updating the checkpoint, window-token estimate, cache epoch, and live handle. Changes with the same message count, including attachment patches and other rewrites, invalidate the candidate. Summary generation and review do not hold the message or event-log lock. A stale automatic commit reports a terminal failure without overwriting newer messages; raw history and the existing direct-compaction summary remain available.

Each scheduled compaction has one operation identity shared by its durable start event, streaming summary frames, terminal event, daemon projection, and transcript outcome. Active summary text is part of `SessionProjection`, so every attached client and a late snapshot observe the same in-flight operation; completed, failed, and daemon-interrupted operations leave terminal transcript entries and are removed from the active collection. Context and run anchors keep simultaneous root and child compactions distinct even when they cover the same message range.

Compaction review policy belongs to the target context. Pending reviews form an ordered collection with independent IDs and context identities; accepting, rejecting, or abandoning one does not replace another. Registration publishes its canonical request and pending snapshot before returning the response future. Dropping that future records abandonment and removes the request. The TUI retains the current review's draft and scroll position across collection updates, then opens the next pending review after resolution. Initial attachment also reads existing pending reviews.

Automatic compaction has a 60-second cooldown per context, measured with a monotonic clock. A fork captures the source cooldown without sharing subsequent updates. Only a successful window commit starts the cooldown; stale candidates and standalone `replace_messages_range` list transformations do not. Manual and overflow compaction retain their forced behavior. Process restart resets the in-memory cooldown.

For session-context calls without an explicit `messages:` override, the runtime
computes a history budget from:

```text
model context budget
- reserved output tokens
- safety margin
- fixed request tokens
  - system prompt and runtime system context
  - tool schemas
= budget available to session messages
```

The prompt is already part of the message list and is counted there. The structured
`input` value is not serialized by the current provider adapters, so neither is
counted again as fixed request input. A large tool registry or system prompt still
reduces the space available to conversation history even when the message list has
not changed.

When the active window exceeds the model-derived trigger, auto-compaction selects an
older contiguous range and asks an LLM for an anchored handoff summary. Range
selection preserves a recent tail using all of these lower bounds:

- at least 10 recent messages;
- at least 5 recent user turns;
- a recent token tail of roughly 5% of the history budget.

The trigger uses a preflight estimate of the current materialized messages plus the
current system/tool prefix. It does not reuse the previous provider call's input count,
which may describe an older window or a different request shape.

The summary replaces the selected range only when the replacement is smaller. Tool
use/result parts are sanitized without dropping valid text or valid pairs from a
mixed message, the replacement is checkpointed, and the live session window is
updated. Depending on `[compaction].review`, manual or all
compactions may be reviewed before commit. If the provider reports an actual context
overflow, the runtime can compact and rebuild the request once before normal retry
handling continues.

Compaction is lossy by design. It preserves an operational handoff, not every detail.
Use history search for older evidence and durable memory for facts that must remain
prominent.

## Durable anchors

Different stores solve different retention problems:

| Store | Prompt behavior | Intended use |
|---|---|---|
| Goal | Runtime synchronizes it to the versioned `session.goal` record | The current objective |
| Plan | Runtime synchronizes it to the versioned `session.plan` record | High-level ordered route |
| Todos | Not automatically injected as a list; available through tools and UI | Concrete execution items inside a plan step |
| Confessions | Managed agent selects relevant records before its main loop | Avoid repeating known failures |
| Rules | Managed agent selects and loads relevant rules before its main loop | Task-specific operating constraints |
| Specs | Available through memory tools | Feature progress and deviations |

Goal and plan stores remain the source of truth. Before a model call, their current
values synchronize into versioned context records. Compaction retains the latest
record per live key, and clearing either store appends a tombstone.

## Managed agent composition

The default agent currently follows this sequence:

1. Push the user request into session history.
2. Load the rule index, confession index, and five recent messages.
3. Ask a cheap extraction call which rules and confession triggers are relevant.
4. Append each selected full rule and matching confession as an independently keyed
   retrieved context record; unchanged items are not appended again.
5. Load the stable managed system prompt from `prompts/system.md`.
6. Enter a `loop` whose main call uses `context: "session"` and the managed system prompt.
7. Dispatch tool calls and push their results into session history.
8. When no tools are requested, check the current flow's pending interjections, classify structured task/transcript/response evidence as complete, awaiting user input, missing an intended action, or incomplete work, then continue or terminate accordingly.
9. Before the root flow terminates, wait for an active watcher event, append a fired event to session history, and continue the loop.

The managed `.at` files own these continuation and termination decisions. The shared classification contract and scoped action/work nudges are loaded from managed prompt files, while the runtime supplies message, watcher, and pending-input primitives without deciding whether an agent loop is finished. Managed templates are compared byte-for-byte and atomically replaced only when bundled content changes.

This is retrieval before the main loop plus full active-session context inside the loop. It is not a fixed ten-message sliding window and not automatic semantic recall.

## Choosing a strategy

- Use `context: "session"` for a conversational agent that should see the compacted
  session window.
- Use `context: "session_recent(n)"` when a deliberately bounded recent slice is the
  correct contract.
- Use explicit `messages:` for isolated calls with a fully controlled prompt.
- Use `prompt:` without context for classifiers, extractors, and one-shot workers that
  should not inherit conversation history.
- Put the stable objective in the goal and the ordered route in the plan.
- Search history when compaction or recency removed evidence needed for the task.
- Keep large structured artifacts in files or stores and retrieve only the relevant
  portion instead of repeatedly injecting them.

## Attachment errors

Provider attachment errors require an HTTP 400/413 response with a recognized image validation code or a message that identifies invalid image data, image decoding, unsupported image formats, or an oversized individual image. Merely mentioning images is insufficient. Model capability errors, message-shape failures, image-count limits, generic request-size errors, authentication failures, rate limits, and server errors retain the original provider error and do not trigger attachment degradation. JSON classification reads the error code/type and message, not echoed request fields or unrelated metadata; unrecognized errors leave attachments intact.

Local image encoding errors carry the failing `MessagePartId` when the caller supplied one. Import failures and remote errors without a concrete image location leave `part_id` unset. The identity is diagnostic metadata, not part of the provider request or the displayed error text.

Only a managed-context LLM request can degrade its stored images. A reported part ID must occur in the actual request. An unlocated rejection can select an image only when all request images share one known part ID and include a user image; ambiguous multi-image failures do not alter history. Bare prompts, explicit `messages:` overrides, and images omitted by `session_recent` cannot target unrelated stored attachments. Local model-capability rejection also leaves images intact.

Root and spawned flows commit the same image-only patch while holding their context's message lock. The patch is also applied to the pending request snapshot, so configured retries use the marker while preserving request-local rewrites. No additional retry is introduced. Repeated errors do not republish an already-applied patch, and attachment notifications retain the calling run identity.

When a managed request exhausts its retries, its compaction guard is released before automatic compaction is scheduled. Failure cleanup must not reacquire a lock still held by that request.

Persisted attachment patches address either a stable image part ID or a legacy message sequence and part index, never both. Only matching image parts are replaced; text and existing replacement markers remain unchanged. Selected context replay applies patches within its ancestry boundary. Legacy spawned and inline runs share their context owner's patch scope, separate from the root history.

Daemon transcript projections retain context and image identities and the source index of checkpoint messages. Attachment updates produce a transcript replacement delta, so attached clients and snapshot recovery receive the same updated content. Older private projection snapshots without this identity state are rebuilt from the event log. A checkpoint event sequence is not a legacy message address.

## Implementation references

- `crates/atman-runtime/src/context_state.rs` — message views, compaction locks, checkpoint epochs, usage, and prefix observations
- `crates/atman-runtime/src/eval/llm_context.rs` — message-source selection
- `crates/atman-runtime/src/eval/llm_dispatch.rs` — final request assembly and compaction entry
- `crates/atman-runtime/src/eval/mod.rs` — runtime system context
- `crates/atman-runtime/src/compaction.rs` — budgeting, range selection, summary, and checkpointing
- `crates/atman-runtime/src/templates.rs` — managed agent composition
- `crates/atman-runtime/src/message_stream.rs` — event stream to active messages

Root and spawned flow compaction share budget calculation, summary generation, review decisions, retained-output rewriting, and source validation. Child creation captures the parent's review policy even when message inheritance is disabled. The review service is shared independently of the message-context owner; accepting or rejecting a child review cannot rewrite the parent window. Summary generation and review waits do not hold the synchronous message lock, and changed source windows reject the candidate before checkpoint publication.
