# Managed agent goals

Successful empty model responses are returned to the calling flow without transport retries. The `llm.call(output: "response")` result contains `message`, `stop_reason`, `has_text` (non-whitespace text), and `has_thinking` (a thinking part is present). Managed agents continue a normally ended response without usable text or tool calls in the current turn. They preserve thinking and prior tool results, omit empty assistant messages from subsequent requests, and add one continuation instruction per consecutive empty-response sequence. Waiting increases from one to eight seconds between calls and resets when text or tool calls resume. Existing execution pauses, cancellation, and subagent iteration limits still apply; empty responses do not complete the goal or publish a final answer. Length-limited and cancelled responses retain their separate handling.

A Goal is a persistent objective with `active` or `paused` state. Each flow owns its Goal independently of its execution state. Goal pause stops objective continuation; it does not stop the current turn, block new input, or pause another flow. A completed turn leaves a persistent instance available for later work.

## Final replies and continuation

Managed agent prompts require every turn to conclude through `final.answer`, including ordinary questions, clarification, and blocker reports. The call contains the complete response in `message` and its summary in `_atman_intent`, with no sibling tool calls. When pausing a Goal, receive the pause result before issuing `final.answer` separately.

`agent.at` and `subagent.at` use the shared `deliver` flow from `user:agent-delivery.at`. It calls `final.answer(message: reply)` before proposing continuation for an active Goal. Final publication does not bypass remaining entry code; the turn finishes after entry execution and its completion hooks return. The scheduler then considers pending work.

A normally completed, non-empty plain-text reply can use this delivery path only if the model has not called any tools during the current turn. After any model tool call, including a failed or invalid call, plain text remains intermediate and the flow keeps reminding the model to call `final.answer`. Template initialization does not count. This policy lives in the `.at` entries; ordinary LLM dispatch does not upgrade text into a final reply.

The entries request unpublished responses with `llm.call(record: false, output: "response")`, inspect the stop reason, and record ordinary tool-call replies before dispatch. `final.answer` accepts a string or assistant Message, publishes and persists the response with its activity summary, and returns its text. A bare `return` passes a value to the caller; entries that need a visible final reply call `final.answer` explicitly.

An entry can publish a reply and explicitly propose its next turn:

```at
flow deliver(reply: any) -> string {
    answer = final.answer(message: reply)
    state = memory.goal.get()
    when state.goal {
        when state.goal.status == "active" {
            memory.goal.schedule(
                goal_id: state.goal.id,
                expected_revision: state.goal.revision,
                message: "Continue the recorded objective from the current context.",
            )
        }
    }
    return answer
}
```

Call this helper with `deliver(reply).await`. Ordinary pending input takes priority over a Goal continuation. A continuation retains the source invocation's arguments and settings. Changing or pausing the objective invalidates an unread continuation. An active Goal alone does not install a scheduler or invent a continuation policy for a custom entry.

## Goal tools

`memory.goal.get` returns `{goal, flow}`: the current objective, or null, and the execution information exposed by `flow.status`. Changes return `{goal, request_id, receipt}`. Existing records require `goal_id` and `expected_revision`; omitting both only permits creation when no Goal exists. An optional stable `request_id` UUID reconciles identical retries. A retry returns its original result without overwriting later changes; a changed payload under that ID fails.

| Tool | Effect |
| --- | --- |
| `memory.goal.get` | Read the current Goal or an authorized peer's Goal |
| `memory.goal.set` | Create an objective or edit its text without resuming it |
| `memory.goal.pause` | Record `completed`, `needs_input`, `blocked`, or `user_requested` with a summary |
| `memory.goal.resume` | Resume an agent-paused objective under the applicable authority rules |
| `memory.goal.schedule` | Propose this instance's next turn for the current active Goal version |

`target_handle` selects a peer for Goal reads and edits. Reading requires context-read or goal-management permission; edits require goal-management permission. Creation relationships grant no implicit control over another flow. Agent operations cannot override a user pause. Scheduling is restricted to the calling instance and requires a persistent host binding.

`memory.goal.schedule` returns `{accepted, acceptance}`. A changed or paused Goal declines the proposal without failing the entry. Invalid identity, permissions, and storage failures remain errors. Acceptance is durable queueing, not proof of another execution, and does not resume paused execution.

Launch requests can supply `initial_goal`. Activation and objective creation share one journal commit. Matching launch retries preserve the original receipt without resetting subsequent Goal edits. Without `initial_goal`, the runtime does not infer an objective from entry arguments.

## Execution and recovery

Managed peers retain their instances after delivering a task. Reuse requires a matching role, project/workspace, authority, task scope, and relevant context. Quiet public output, conversation turn counts, and directory activity timestamps do not establish elapsed execution time or a stalled peer. Async spawn already routes final replies to its creator; additional final-result polling or subscriptions are unnecessary. Retain the request and actual turn identity, and query `flow.result(handle, turn)` when checking a particular assignment. Public `flow.output` history can include earlier assignments.

Async `flow.spawn` returns its recorded `first_turn` and `first_run`. Later assignments use `flow.interject`'s request ID with `flow.status(handle, request_id)` to inspect `request.delivery` and `request.receiver.input`. Receiver input is `pending`, `cancelled`, or `committed`; committed input includes its actual lease and receipt. Several requests can belong to one turn, so consumption does not prove that each task succeeded. Queries inspect only the current sender's own request, recheck live permissions, and preserve separate sender and receiver receipts. Control-authorized status readers also receive `control.pending_input_count`; discovery alone does not expose that count. Recover can admit queued inputs immediately.

Before taking over a peer's writable area, confirm the stop applied and account for related descendants and background writers. Reconcile existing files and uncertain external effects before continuing. Recovery releases an interruption gate and may immediately admit existing pending inputs; it does not resume the failed call stack or replay its effects. Closed instances retain history but cannot reopen. Authorized replacement work uses a new instance and explicit results, artifacts, effects, and remaining-task handoff without reviving cancelled scopes or overriding user pauses.

Managed system guidance refreshes on initialization. Role prompts update only when absent or when their complete contents match a known factory version; custom and symlinked role files are preserved with a diagnostic. Roles follow their actual tool allowlist. Research reports can include bounded open questions, verification reports can describe an unsuccessful reproduction, and implementation checks follow the affected scope and explicit resource constraints.

Activity uses **Working first** by default, followed by newer registrations within each state group. **Newest first**, **Oldest first**, and **Name A–Z** are available through the persistent **Sort** button. Multiple pages place sorting alongside Previous/Next; single-page, empty, and error views retain the sorting control. Narrow or short panels use a compact layout without removing Sort; expanded sidebars that cannot fit their content use the full auxiliary page. Focus the Activity tab, use Tab or arrow keys to reach its controls, and press Enter or Space to activate; keyboard focus reveals the selected control, and clicking the composer returns focus to input. The shared selection form applies sorting locally and Escape cancels it. Main and floating views retain independent choices, preserve selected task identities across sorting, and do not reorder for token previews or spinner frames. Background task kinds retain their grouping and sort within each group; display sorting does not change execution order.

Goal operations and execution controls are separate. A flow can be running with a paused Goal or idle with an active Goal. Goal editing, replacement, clearing, and pause do not change an execution gate or another flow's objective. `flow.control_batch` provides revision-bound pause, resume, cancel-turn, recover, and close operations under explicit grants.

Execution pause blocks new admission and cooperative execution boundaries. Resume continues the paused execution; it is not Goal resume. `flow.kill` cancels the current turn without deleting the instance. `flow.close` closes the instance and releases its resources after execution stops; its history remains readable. Stopping one instance does not implicitly stop its creator or peers.

The instance journal stores Goal identity, revision, timestamps, status, and summary alongside execution and inbox facts. Opening an instance whose recorded turn lost its executor marks that turn interrupted; it does not replay external effects. Recovery requires explicit execution control. A read-only inspection does not perform this recovery. See [Storage](./storage.md) for ownership, journal layout, and recovery contracts.

User Goal resume atomically restores a paused objective and accepts one `GoalResume` input. An identical retry does not queue another turn; a new resume request for an already active Goal is rejected. The input retains the objective version and last invocation settings. Editing, pausing, replacing, or clearing the objective cancels its unread resume input. Paused or interrupted execution retains the input until execution is explicitly resumed or recovered. The command itself does not bind a scheduler.

Goal timestamps are stored in UTC. Model calls receive the host's local clock and UTC offset through a runtime context record. On remote hosts this may differ from the user's timezone. Pause summaries are model-authored text and remain unchanged; they are not reformatted timestamps.

No default turn, token, fuel, or time budget is added. A live execution host is required for scheduling; stored state does not recreate suspended Rust tasks or replay process commands after restart.

## Peer results and watchers

Async spawn creates a retained peer and installs a result subscription to the creating flow. During an active turn, its next general model call can consume results. When idle, pending results can start a new turn. Synchronous spawn waits for the first turn, not the instance lifetime; its returned result is not also delivered as an asynchronous notification. Later turns can publish new results.

Flow watchers observe public final replies, turn completion, or instance closure. A final-reply pattern filters that public reply, not private reasoning or intermediate tool output. Spawn-result subscriptions and explicit watchers share persisted observation and inbox handling. A matching fact is delivered with its applicable subscription identities rather than starting independent duplicate work for each matching route.

Each subscribing flow owns its inbox. General model calls and explicit watcher waits consume through the same delivery mechanism; isolated auxiliary calls do not consume notifications. Final publication does not wait for watchers. Inputs not consumed before turn completion remain pending for later admission. Execution pause retains them; Goal pause alone does not prevent handling. Notification content is data, not a new user instruction or authorization.

Persistent watchers have no default lifetime limit. An optional `timeout_ms` sets expiry. Unwatch stops future observation but preserves already accepted notifications. Public watcher listing and cancellation exclude internal spawn-result routes. Process subscriptions observe owned live resources; restarting a host does not rerun their commands. Lost sources are reported through the persisted observation mechanism.

## TUI and CLI

The main view and floating views bind to flow instances and share Goal controls. `:goal` or **Manage Goal** in `Ctrl+P` opens the bound flow's Goal panel. The Goal card and `Ctrl+Y` toggle Goal pause/resume, independently of execution controls. Select another flow through FlowHub to control its objective.

`Tab` / `Shift+Tab` selects controls, `Enter` activates them, and `Space` toggles the Goal outside editing. Mouse controls use the same actions. **Save** and `Ctrl+S` leave editing after persistence succeeds; failures retain the draft and display the reason. **New goal** or `N` creates a replacement objective with a fresh identity, while **Edit** preserves its identity and pause state. Both use the captured revision to reject stale edits. Clear requires confirmation.

The panel opens centered and supports dragging and resizing. Closing a floating view with `Esc` only closes the view. Navigation and view closure do not cancel the bound flow. Use explicit execution controls to pause, cancel, recover, or close an instance.

Plain REPL commands are `:goal`, `:goal set <text>`, `:goal pause`, `:goal resume`, and `:goal clear`. They address the selected flow through the same persistent owner. Setting or clearing an objective does not cancel execution. The interactive Goal-resume path binds the entry before accepting its resume input.

Final replies fold preceding work within the same turn and remain outside that fold. Streaming previews, committed replies, and restored history share the projection; main and floating views retain separate disclosure state. Goal continuation inputs have internal origin and remain hidden as user chat, while their model protocol role remains `user`. Consumed peer and watcher notifications retain their source identities.

## Daemon

`run_flow` and `goal.run` launch persistent instances and return `session_id`, `instance_id`, and the initial `run_id`. `goal.run` requires a managed invocation contract with a non-empty user-message parameter, creates an initial Goal from it, and also returns `goal_id`. Later turns have separate run identities. `cancel_run` addresses the current run without closing its retained instance.

After `session.open` or `session.create`, `goal.get` accepts `session_id` and optional `instance_id`; omission selects the Session's default flow. `goal.set`, `goal.pause`, `goal.resume`, and `goal.clear` require an explicit instance, stable `request_id`, and the current `goal_id`/`expected_revision` pair, or neither version field when creating an absent Goal. Legacy `target_handle` is not an instance address for these RPCs.

Mutations return `{flow, goal, execution, committed}`. Current state and the original command receipt are separate because subsequent execution may already have changed the objective. Authentication and owner validation apply before access. Goal resume queues input but does not implicitly install an execution binding or lift execution pauses.
