# Context strategy

Atman separates durable history, the active message window, workflow-selected memory, and the final provider request. In persistent Sessions, each Flow owns its context independently. `context: "session"` selects the invoking Flow's message window, not every conversation in its containing Session. Selecting a default Flow or moving a view does not merge contexts.

## The request model

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
Ordinary queued input and answers submitted after a `form.ask` timeout are claimed only by a task-facing general `llm.call` and added as trailing user messages, not to the stable system prefix. Late form answers are recorded before consumption and survive session replay; a consumed answer is recorded with its user message in one event. Internal classification, extraction, and compaction calls do not claim them.
Provider adapters serialize these components into the provider-specific wire format.
Explicit tool selectors retain their declared order, wildcard matches are sorted by
qualified name, and overlaps are removed. Tool schema object keys are recursively
canonicalized, so registry insertion order does not change the provider prefix.

Before dispatch, the runtime wraps the assembled request in a provider-neutral `ModelContextPlan`. Its `ContextPlanId` is recorded on the corresponding `llm_call` event and does not alter provider serialization. The identifier correlates the compiled plan with its resulting call event. The event separates estimated stable-instruction, tool-definition, message, and context-record tokens. Provider usage remains authoritative when present; missing input or output counts use the complete plan estimate, with provider, estimated, or mixed usage identified explicitly.

Calls are classified by purpose and context identity, with bounded provider/model/purpose/identity usage buckets. A general call in a persistent Flow's own context updates that Flow's active model-window reading. Auxiliary calls do not replace it. The serialized `root` scope identifies an owning context, not a privileged Flow: every persistent instance, including one created by another Flow, owns such a context. The `child` scope remains available for isolated spawned history segments in embedded legacy execution.

Each call also fingerprints the cacheable prompt sequence after OpenAI Chat, API-key Responses, Anthropic Messages, Codex Responses, or provider-neutral projection. The observation records prompt bytes and estimated tokens, plus a conservative common-prefix lower bound at complete tool, instruction, message, or content-block boundaries. It reports cold starts, cache enablement or disablement, provider/model/projection changes, stable-instruction changes, tool changes, compaction, and other message-prefix rewrites separately. Transport JSON punctuation and output-only request fields are excluded because they do not represent the provider's semantic prompt prefix. Successful calls record the number of tool uses returned in the assistant message. Each dispatched tool result records raw bytes, model-visible excerpt bytes, and whether output budgeting changed the content before it enters message history.

API selection resolves from the configured model and its provider before converting an alias to an API model ID. Request dispatch, reasoning validation, and cache projection use the same model configuration snapshot and selected provider. Compaction uses the same protocol resolution. Changing a model between Chat Completions and Responses changes its projection; a stable cache routing key does not guarantee a provider cache hit.

Responses uses `store: false` and replays local message history. Opaque reasoning items and output metadata remain attached to assistant messages with their provider, endpoint, and model identity. Matching routes can replay them after session restoration; other routes omit the opaque state. Compaction removes this state with the corresponding messages, and it is not rendered as user-visible text.

Completed streaming items own their replay state; a terminal response summary cannot replace them with a partial copy. A stream must reach a valid terminal response before tool calls can execute. When a terminal response omits its output array, every retained item must already have a completion event. Transport interruptions can be retried without dispatching partial tool calls; malformed protocol data remains an error.

Dynamic model context can be represented as an internal `ContextRecord` message part. Records carry a stable key, monotonic revision, semantic content digest, authority, and retention policy. Ordinary records retain their existing provider roles: OpenAI chat uses mid-conversation system messages, Codex Responses uses developer input items, and Anthropic uses explicitly framed user context. The `Reference` body owns reference text and typed images and always projects as user-level retrieved data. Its images contribute to token estimates and content digests and are replaced with their owning record by Latest selection. Latest-value records survive compaction and remain hidden from the normal TUI transcript while staying present in event and checkpoint data.
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

`context_records: ["session.goal", "session.plan"]` selects exact context-record keys for an individual LLM request, including extraction and classification calls. An empty list excludes records; omitting the argument preserves the existing record projection. Ordinary messages remain governed by `context` or `messages`. Filtering affects only the request, including requests rebuilt after compaction; stored messages and sibling calls retain their records.

`context_latest: ["quickask.source"]` projects only the last `Latest` record for each selected key into that request. A latest tombstone removes the selected key; `Timeline` records, other keys and ordinary messages retain their existing projection. The option does not change stored history or sibling calls. It composes with `context_records`, applies again after context-overflow compaction, and includes the selected record revisions and digests in the request's cache epoch. Omitting the option or passing an empty list preserves the existing epoch. Replacing a selected source therefore resets that request's cache route without changing the session's compaction epoch.

The managed `commands/quickask.at` entry uses its own question prompt, explicit invocation effort, `cache: true` and the latest `quickask.source` projection. Its model tools are file reading, listing, search, images, registered output reading and final delivery. It does not invoke agent naming or Goal continuation. The host must separately supply and enforce the question's read-only authority, source capture and lifetime; selecting this recipe alone does not create a temporary question or grant access to another conversation. Provider cache misses remain valid outcomes, including the first question after capturing a source.

Budgeted compaction preserves the latest context records and tombstones when replacing individual turn outputs, including records appended after user messages. Hiding a record from a request does not remove it from subsequent calls or let cleared records reappear.

Flow contexts also receive `runtime.clock`: the runtime host's local date, time, and numeric UTC offset at minute precision. It refreshes before model calls when the minute or offset changes, including after session recovery. Repeated calls within the same minute reuse the record; clock updates append to history without rewriting the stable system prefix. The record distinguishes UTC timestamps from local time and identifies its timezone as the host's, which may differ from the user's. It is a current clock sample, not evidence of when a past action occurred.

The stable system template retains the agent identity, voice, durable work contract, and general tool-use workflow. Because this prefix is stable across calls, its length affects cold-start input and the context window but does not invalidate later cache prefixes. Context state, retrieved rules, confessions, and changing capabilities stay in append-only records instead of rewriting the template. The template has no working-directory placeholder. Each persistent Flow obtains its workspace record from its bound execution context; isolated legacy history segments use their effective tool workspace.

## Message selection

Native hosts can capture a bounded reference with `RuntimeHost::capture_reference`; model-facing callers use the same path through `FlowClient` with a live `ReadContext` grant. Capture selects one committed source position and checkpoint, bounds metadata, messages and retained artifacts separately, and performs payload reads on the shared read workers. Only an already opened source owner is eligible. The returned snapshot owns its readable image and registered-output content independently of subsequent source deletion, reports omissions explicitly, and contains no running tools or inherited control state. It is a reference at a commit boundary, not a complete-turn fork or an instruction to execute the source conversation.

`capture_reference_at_boundary` resolves a selected user turn or successful completion against an exact source observation in the same owner request. A before-turn boundary uses the real preceding journal commit; batched and inserted inputs select their entire containing invocation. An after-turn boundary requires the original successful completion receipt, so a live final answer or interrupted turn cannot qualify. The result remains a bounded reference with explicit omissions, rather than an executable copy of the complete history.

`ReferenceSnapshot::into_source_record` compiles that data into one `quickask.source` record under an explicit estimated-token budget. Source roles and tool actions become quoted data; reasoning signatures, opaque Responses continuation state and input presentation metadata are excluded. Images remain typed visual inputs rather than Base64 text. Additional context or image omissions are reported explicitly. `into_source_record_at` also retains the authenticated Flow, branch, cursor and omissions in typed reference-origin metadata; provider text and image parts exclude this metadata.

`RuntimeHost::prepare_question` resolves a concrete model, reads the captured recipe's actual policy and tool specifications, and reserves the existing model compaction headroom plus question and runtime-context estimates before capturing its reference. The retained preparation fixes creation, source and invocation identities for launch retries. Its authority intersects the source ceiling with current host authority and the read-only limit; its project binding uses the actual source Flow. Subsequent turns submit the concrete model as an explicit entry argument without changing global aliases. Source refresh prepares a same-branch record and updates the idle question through a real context checkpoint without starting a user turn. The [Quick Ask interface](quick-ask.md) uses these shared native execution and retention operations.

`validate_question_model_switch` checks an existing question before accepting a different model. It reads the owner's current model window, applies the same latest-source projection as model dispatch, and estimates the retained reference, conversation, runtime records, actual policy and tools against the target model's existing reserves. An insufficient window keeps the previous selection and directs the user to refresh the reference or start a new question. A concurrent context or project change requires retrying the selection; token estimates remain distinct from provider-reported usage.

Recipe source is captured in its revision bundle. A policy loaded through `@` retains the ordinary file-reference semantics: preparation measures its current bytes, and execution reads the file again. Dispatch recalculates the actual fixed input budget and uses the existing context compaction and overflow recovery; preparation does not promise that a subsequently changed or oversized policy will fit.

The flow chooses one message source:

| Form | Messages sent |
|---|---|
| `messages: [...]` | The latest live runtime records followed by the explicit message list. Cannot be combined with `prompt:` or `context:`. |
| `context: "session"` | The current session message window. An optional `prompt:` is appended as a user message. |
| `context: "session_recent(n)"` | The latest live records followed by the last `n` non-record messages from the current session window. |
| `prompt: "..."` | The latest live runtime records followed by one user message, with no conversational history. This is also the default when `context:` is omitted. |

The managed `commands/agent.at` uses `context: "session"`. It does **not** feed `memory.recent_turns` to the main model as a fixed sliding window.

At the start of the managed flow, `memory.recent_turns(n: 5, excerpt_mode: "conversation", excerpt: { head: 4000, tail: 8000 })` provides a bounded conversation excerpt to the cheap rule/confession selector. `head` and `tail` are independent character budgets selected in `agent.at`; when the middle is omitted, `excerpt` keeps both edges in transcript order. The current request is supplied separately in full. The lossless `items` remain available to explicit callers, and these helper calls do not define the main model's session context. The older `excerpt_chars` input remains available for recent-first excerpts and cannot be combined with `excerpt`.

## Session history and active window

The durable event stream is the source of session history. The runtime derives the
message stream from user, assistant, and tool events, plus checkpoints. The active
window is the message list currently used by `context: "session"`.

Before compaction these are effectively the retained conversation messages. After
compaction, the active window contains a structured compact summary followed by
recent messages. A checkpoint persists that replacement and synchronizes the live
message handle, so subsequent `context: "session"` calls use the compacted window.

Spawned flows keep a separate message segment. With `inherit_context: true`, the child starts from a copy of the parent's active window and then appends its invocation message. That copy contains only complete tool-use/result pairs, so the active parent `flow.spawn` transaction and other concurrently executing tools do not become orphaned requests in the child context.

By default, successful `llm.call(context: "session")` calls append their assistant message to the active root or child segment. Flows using `record: false` take responsibility for recording that response. Managed templates use `session.push(reply)` for ordinary responses and `final.answer(message: reply)` for final delivery, then separately record tool results and interjections.

Child segments use the same token-aware, transaction-aligned compaction and checkpoint mechanism as root history. No message-count trim rewrites the child prefix outside compaction; each child retains its own context epoch and cache-prefix observations.

Published async child final answers enter a scope-bound inbox belonging to the direct parent. A general model request reserves the pending FIFO batch and records it with `child_result` origin at pickup; isolated and auxiliary calls do not consume it. Root and child requests use the same ownership mechanism. If the parent is idle, the host records the batch in a new turn instead. Request preparation failure releases reservations, and ending a turn does not discard unread child results. Results remain untrusted task data and cannot authorize a goal resume or a privileged operation as user input.

Tool results use the same configured line, byte, and per-line budget in dispatch,
`session.push`, child segments, and direct Session appends. The Executor projects the
active budget into the Session at root-flow start, while spawned ToolCtx values inherit
the same snapshot.

`OutputStore::memory` provides writable registered outputs without a filesystem directory. Captured immutable outputs enter through `import_retained`; matching identities are reused, conflicts and capacity failures publish nothing. New outputs and captured seeds share the Flow's transient retention budget. Read views and `memory_snapshot` retain allocation ownership until their final consumer releases it. A frozen memory snapshot can copy all registered output identities into durable storage for Keep without executing tools or reading from the original source directory.

`RuntimeHost::launch_transient_entry` uses the native worker, journal validation, reader and scheduler without creating a Flow directory or a persistent catalog row. Repeated creation requests reconcile the existing owner. The host allows 128 MiB of retained payload in total; each temporary Flow allows 32 MiB, including 256 KiB reserved for bounded failure and closure records. A capacity failure stops the active invocation while preserving readable history. `discard_transient` closes input admission, stops scheduling and execution, and releases the owner; retrying the same discarded identity cannot recreate it. Host shutdown disposes temporary owners, while closing an observing window does not control their execution. Explicit [Keep](flow-retention.md) transfers the retained history and coordinates durable execution and attachment.

Native question drafts have a distinct `ViewAddress::QuestionDraft` with no Session or worker. Launch and Keep attachments retain their original view, editor and window, validate the exact operation ticket, and install only a completely prepared document. Launch requires a transient document; Keep requires a durable document with the same Flow identity. Hidden handoffs preserve focus, and superseded reply mailboxes drain without applying old destination changes to the replacement. Attachment failures retain the accepted execution and original ticket; retries rebuild observation without resubmitting input. Only the installed view acknowledges its new binding and retires the old route.

Before provider projection, one canonical tool-pair normalizer moves every result next
to its assistant call, emits parallel results in call order, fills interrupted calls,
and removes only orphan result parts. Ordinary text in mixed messages remains visible.

Each root invocation also owns a transient request-exposure registry. Successful LLM
responses register only tool-use IDs and canonical names that appeared in that request's
tool definitions. `dispatch_all` claims the matching flow-scoped entry before consulting
the global registry; unexposed, renamed, duplicate-ID, cross-flow, and replayed calls are
returned as tool errors without execution.

Within one assistant response, `dispatch_all` treats calls as a candidate parallel frontier. Each tool resolves structured resource claims once from `ToolArgs` and `ToolCtx`; independent calls can run concurrently, conflicting calls retain source order, and permission-control calls split authorization epochs. Primary results and supplemental model messages remain in original call order. Calls whose arguments or correctness depend on an earlier result belong in a later model response or an `.at` flow using variables and `.await`; `fanout` is reserved for independent branches.

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

The trigger uses a preflight estimate of the current materialized messages plus the current system/tool prefix. A successful root call calibrates later fresh estimates for the same provider and model, so provider tokenizer differences are retained without treating an older request total as the current window.

The summary call resolves the same configured model ID as a normal `llm.call`. If the anchored summary request fails, the selected range remains in place and a failed compaction event is emitted; an unavailable anchored summary is never committed as a placeholder. A successful summary replaces the selected range only when the replacement is smaller. Tool use/result parts are sanitized without dropping valid text or valid pairs from a mixed message, the replacement is checkpointed, and the live session window is updated. Depending on `[compaction].review`, manual or all compactions may be reviewed before commit. If the provider reports an actual context overflow, the runtime can compact and rebuild the request once before normal retry handling continues.

Hosts can call `compaction::compact_now` for an explicit user request, including when execution is paused. It acquires the same context lock and uses the same summary, review, and checkpoint path as automatic compaction without resuming execution or changing the Goal. `CompactOutcome` distinguishes a committed replacement, an unchanged window, and a rejected review; provider and storage errors are returned to the caller. Dropping a pending review releases only that request. Persistent contexts commit through their existing flow writer and retain the complete original transcript for history and restart recovery.

Compaction is lossy by design. It preserves an operational handoff, not every detail.
Use history search for older evidence and durable memory for facts that must remain
prominent.

## Durable anchors

Different stores solve different retention problems:

| Store | Prompt behavior | Intended use |
|---|---|---|
| Goal | Runtime synchronizes it to the versioned `session.goal` record | The current objective |
| Child goals | Runtime synchronizes descendant revisions before the parent's next model request | Delegated objectives, pause reasons, and completion summaries |
| Plan | Runtime synchronizes it to the versioned `session.plan` record | High-level ordered route |
| Todos | Not automatically injected as a list; available through tools and UI | Concrete execution items inside a plan step |
| Confessions | Managed agent selects relevant records before its main loop | Avoid repeating known failures |
| Rules | Managed agent selects and loads relevant rules before its main loop | Task-specific operating constraints |
| Specs | Available through memory tools | Feature progress and deviations |

Goal and plan stores remain the source of truth. Before a model call, their current
values synchronize into versioned context records. Compaction retains the latest
record per live key, and clearing either store appends a tombstone.

## Managed agent composition

`memory.recent_turns(..., excerpt_mode: "conversation")` produces an excerpt from user messages, assistant conclusions (using saved final-answer summaries when available), tool-call intent, compact summaries, and bounded tool errors. It omits successful tool-result bodies, thinking, and context records. The existing `excerpt` head/tail bounds or `excerpt_chars` budget apply after this projection. Returned `items` and stored history remain lossless; the default `transcript` mode is unchanged. No model call is made to produce this projection.

The default agent currently follows this sequence:

1. Push the user request into session history.
2. Load the rule index, confession records, and a conversation excerpt from five recent turns. Project rule names/descriptions and confession triggers/violated rules into a compact catalog, also stored under `agent.selection.catalog` for on-demand retrieval by the main model.
3. Ask a cheap extraction call which rules and confession triggers are relevant. The request includes only `agent.goal`, `session.plan`, and `agent.selection.matches` records alongside the compact catalog, conversation excerpt, and current request. Its system instruction treats this material as selection data and prioritizes the current request over older tasks. Validate selected identifiers against the catalog and save them under `agent.selection.matches`. If extraction fails, `agent.at` explicitly falls back to an empty selection and continues; existing retrieved rules remain intact and the main model can fetch relevant content from the compact catalog. Failure does not trigger full-catalog expansion or another summarization call.
4. Append each selected full rule and matching confession as an independently keyed
   retrieved context record; unchanged items are not appended again.
5. Load the stable managed system prompt from `prompts/system.md`.
6. Enter a `loop` whose main call uses `context: "session"` and the managed system prompt.
7. Dispatch tool calls and push their results into session history.
8. Explicitly call `final.answer(message: reply)` and return for a valid final control. A normally completed, non-empty plain-text reply is accepted only before any model tool calls in the current turn. After tool calls, record the candidate and request an explicit `final.answer` until a valid call arrives. Persist ordinary tool-call replies before dispatching tools. Preserve interrupted or truncated replies without marking them final.
9. After the turn ends, the host prioritizes queued user input, then delivers pending watcher notifications in a new turn or continues an active goal. Watcher notifications are batched in arrival order, retain their watcher origin, and respect execution pauses.

The managed `.at` files own tool loops and final-answer acceptance. The host owns turn boundaries and background wakeups; watcher events do not keep a finished loop alive. Managed templates are compared byte-for-byte and atomically replaced only when bundled content changes.

`llm.extract(fallback: {...})` lets a flow explicitly select a replacement result when the provider fails or the response still fails field parsing after any configured parse retries. The replacement is validated against `fields` before the request. Without `fallback`, extraction errors propagate. Cancellation, watcher aborts, redirects, and turn restarts always propagate; a fallback does not suppress execution controls. Using a fallback emits a warning and does not convert the invalid model response into a tool invocation.

Array fields validate each element using the declared scalar type's coercion rules. An incompatible element invalidates the entire extraction result; a partially valid array is not treated as a successful selection. Explicit fallback arrays follow the same validation before any model request.

`llm.call(record: false)` leaves the returned assistant response unrecorded and unnormalized so a flow can choose how to publish it. Context reads, queued input pickup, usage accounting, and pre-request compaction retain their usual behavior. `output: "response"` returns `{message, stop_reason}` instead of the default value; stop reasons are `end`, `tool_use`, `length`, and `cancelled`. These options are independent and default to `record: true` and `output: "value"`. Managed templates select both to avoid recording the same reply as ordinary text and as a final answer. Custom flows must explicitly persist any unrecorded reply they intend to retain, using `session.push` or `final.answer`.

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

## LLM response handling

`llm.call(record: false, output: "response")` returns `message`, `stop_reason`, `has_text`, and `has_thinking`. `has_text` requires at least one non-whitespace character in the combined text; `has_thinking` indicates a thinking part is present. A successful response with no parts remains a successful call and does not enter the transport retry policy. The calling flow decides whether to record the response, continue execution, or deliver an answer.

## Implementation references

- `crates/atman-runtime/src/eval/llm_context.rs` — message-source selection
- `crates/atman-runtime/src/eval/llm_dispatch.rs` — final request assembly and compaction entry
- `crates/atman-runtime/src/eval/mod.rs` — runtime system context
- `crates/atman-runtime/src/compaction.rs` — budgeting, range selection, summary, and checkpointing
- `crates/atman-runtime/src/templates.rs` — managed agent composition
- `crates/atman-runtime/src/message_stream.rs` — event stream to active messages
