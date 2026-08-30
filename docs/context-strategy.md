# Context strategy

Atman separates durable session history, the active message window, workflow-selected
memory, and the final provider request. Treating all four as one "context window"
hides important behavior.

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
The Session retains the latest usage in bounded provider/model/purpose/identity buckets.
Only a general root-session call updates the active model-window reading; helper and
child calls remain visible in their own buckets without replacing it.

When an `llm.call` runs with a session runtime, the runtime appends goal,
working-directory, active-plan, and model information to its system prompt, even if
the call itself uses an isolated `prompt:` rather than session messages. The managed
agent adds a second, workflow-owned layer: it reads `prompts/system.md`, selects
relevant rules and past confessions, and includes those in its own `system:` value.
Rules and confessions are therefore selected by the managed workflow, not injected
automatically by every `llm.call`.

The stable system template does not contain a working-directory placeholder. Root
calls receive one working-directory block from session context; spawned flows receive
one block resolved from their effective tool workspace. This prevents duplicate root
context and literal placeholders in child requests.

## Message selection

The flow chooses one message source:

| Form | Messages sent |
|---|---|
| `messages: [...]` | Exactly the explicit message list. Cannot be combined with `prompt:` or `context:`. |
| `context: "session"` | The current session message window. An optional `prompt:` is appended as a user message. |
| `context: "session_recent(n)"` | The last `n` messages from the current session window. |
| `prompt: "..."` | One user message, with no session history. This is also the default when `context:` is omitted. |

The managed `commands/agent.at` uses `context: "session"`. It does **not** feed
`memory.recent_turns` to the main model as a fixed sliding window.

At the start of the managed flow, `memory.recent_turns(n: 5, excerpt_chars: 12000)`
provides a bounded excerpt to the cheap rule/confession selector. The same bounded
view is used later by the stall classifier. Its lossless `items` remain available to
explicit callers, and these helper calls do not define the main model's session
context.

## Session history and active window

The durable event stream is the source of session history. The runtime derives the
message stream from user, assistant, and tool events, plus checkpoints. The active
window is the message list currently used by `context: "session"`.

Before compaction these are effectively the retained conversation messages. After
compaction, the active window contains a structured compact summary followed by
recent messages. A checkpoint persists that replacement and synchronizes the live
message handle, so subsequent `context: "session"` calls use the compacted window.

Spawned flows keep a separate message segment. With `inherit_context: true`, the
child starts from a copy of the parent's active window and then appends its invocation
message. That copy contains only complete tool-use/result pairs, so the active parent
`flow.spawn` transaction and other concurrently executing tools do not become orphaned
requests in the child context.

Ephemeral child segments retain the existing 100-message target. Trimming moves the
cut backward when necessary to keep a retained ToolResult with its ToolUse, so one
complete tool batch may temporarily make the segment slightly larger than the target.
An active unpaired ToolUse at the tail is preserved until its result is appended.

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
| Goal | Runtime appends it to every session-backed LLM system prompt | The current objective |
| Plan | Runtime appends the active plan to the system prompt | High-level ordered route |
| Todos | Not automatically injected as a list; available through tools and UI | Concrete execution items inside a plan step |
| Confessions | Managed agent selects relevant records before its main loop | Avoid repeating known failures |
| Rules | Managed agent selects and loads relevant rules before its main loop | Task-specific operating constraints |
| Specs | Available through memory tools | Feature progress and deviations |

Goal and plan survive message compaction because they are stored outside the message
window and reassembled into the system prompt. Clearing either store removes that
anchor from later calls.

## Managed agent composition

The default agent currently follows this sequence:

1. Push the user request into session history.
2. Load the rule index, confession index, and five recent messages.
3. Ask a cheap extraction call which rules and confession triggers are relevant.
4. Load the selected full rule text and matching confession records.
5. Build the managed system prompt from `prompts/system.md` plus selected context.
6. Enter a `loop` whose main call uses `context: "session"` and the managed system prompt.
7. Dispatch tool calls and push their results into session history.
8. When no tools are requested, classify whether the agent is done, blocked, lazy, or forgot tools; continue or break accordingly.

This is retrieval before the main loop plus full active-session context inside the
loop. It is not a fixed ten-message sliding window and not automatic semantic recall.

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

## Implementation references

- `crates/atman-runtime/src/eval/llm_context.rs` — message-source selection
- `crates/atman-runtime/src/eval/llm_dispatch.rs` — final request assembly and compaction entry
- `crates/atman-runtime/src/eval/mod.rs` — runtime system context
- `crates/atman-runtime/src/compaction.rs` — budgeting, range selection, summary, and checkpointing
- `crates/atman-runtime/src/templates.rs` — managed agent composition
- `crates/atman-runtime/src/message_stream.rs` — event stream to active messages
