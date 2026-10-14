# Managed agent goals

A goal is a persistent objective with `active` or `paused` state. A managed agent completes its current turn normally. An active goal permits another turn; `memory.goal.pause` stops automatic continuation and lets the current turn finish its response. Ordinary custom flows remain single executions unless they declare `contract { invocation { goal: true } }` and run through a goal-aware host.

Managed root and child prompts require every turn to conclude through `final.answer`, including ordinary questions and requests for clarification. The call contains the complete response in `message` and its summary in `_atman_intent`, with no sibling tool calls. When pausing a goal, the agent must receive the pause result before issuing `final.answer` separately; pausing alone does not deliver the final response.

Managed `agent.at` and `subagent.at` explicitly call `final.answer(message: reply)` for a valid final control or a normally completed reply with non-empty text and no tool calls. They request an unrecorded response through `llm.call(record: false, output: "response")`, inspect its `stop_reason`, and record ordinary tool-call replies before dispatching their tools. `final.answer` accepts a string or an assistant Message, publishes the final response with its activity summary, persists it in the calling context, and returns the answer text. The templates return immediately afterward. No additional model request is needed, and ordinary LLM dispatch never upgrades plain text automatically. This completion ends the current turn without pausing an active goal; watcher notifications start a separate turn.

A valid `final.answer` is published immediately and ends the current turn. Watcher notifications cannot discard or delay that answer. The interactive host, managed `atman run`, and daemon `goal.run` deliver queued watcher notifications together, in arrival order, in a new turn with a new turn ID and `watcher` origin. A completed one-shot watcher can still have a pending notification. Watcher turns retain the managed invocation settings and do not resume a paused goal; user, parent, cancellation, and failure pauses prevent background turns. The interactive host handles queued user input before watcher or goal continuation. Canonical sub-agent roles do not expose watcher tools; their final answers already end their current turn.

## Goal tools

`memory.goal.get` returns `{goal, execution, paused_by, running_operations}`. `goal` is null when no objective exists. Records include `id`, `revision`, `objective`, `owner`, `parent_goal_id`, `status`, and `pause`.

Goal timestamps remain stored in UTC. Root and child agents receive a runtime clock context record containing the host's local date, time, and UTC offset, refreshed at minute precision before model calls without changing the system prompt. The host timezone can differ from the user's timezone, especially on remote servers; agents are instructed to use the user's timezone when known and otherwise label times with their UTC offset. Pause summaries are model-authored text displayed unchanged; existing summaries are not rewritten by this clock context.

| Tool | Arguments | Effect |
| --- | --- | --- |
| `memory.goal.get` | Optional `target_handle` | Read the current agent or a descendant |
| `memory.goal.set` | `text`; existing goals require `goal_id`, `expected_revision` | Create an objective or edit its text without resuming it |
| `memory.goal.pause` | `goal_id`, `expected_revision`, `reason`, `summary` | Report completion, blocked work, or missing input and stop continuation |
| `memory.goal.resume` | `goal_id`, `expected_revision` | Resume a descendant, or an agent-paused goal after real user input |

Pause reasons accepted from agents are `completed`, `needs_input`, `blocked`, and `user_requested`. All tools accept an optional `target_handle`. A child cannot address its parent or siblings. Stale revisions fail without changing the goal. A new user request can replace an agent-completed goal with a new identity; editing unfinished work preserves its state. User pauses cannot be overridden by agent tools. Managed agents use pause rather than `memory.goal.clear`.

## Execution boundaries

Watcher registration validates its target before adding an active watch. Before returning success, it establishes the source subscription and captures the terminal screen baseline; subsequent screen updates remain available even before the background consumer is first polled. Unwatch stops future results from that registration; notifications already queued remain available. Watcher completion and notification enqueue share one state lock, and changes wake every waiter, including hosts waiting for all watches to finish.

Persistent watchers retain their output subscription and consumed position. Flow and background-command matches advance through available output, including patterns split across updates. Terminal watchers report newly visible matching positions; an unchanged match does not repeat on unrelated output, but removing and displaying it again can trigger another notification. A watcher can match existing output at registration. Source exit ends the watch after remaining matches; broadcast lag recovers from the available output snapshot. An empty pattern is rejected. Match notifications contain the matched text rather than a copy of the complete output history. Use the source output tools when more context is needed.

Background readiness checks do not consume notifications. Hosts prepare the invocation before starting a turn through the shared runtime entry point, which waits for context access, rechecks execution pauses and the current turn, then consumes notifications and records the new turn without another await. Cancellation during that wait leaves the notifications queued. Cancellation and runtime failures block automatic wakeups even when no goal exists; clearing an objective preserves execution pauses, and a real user submission can start work again.

User Pause stops new requests and tool starts. An in-flight model request finishes and its reply is recorded before execution waits. Already running tools can finish; undispatched work waits. Resume continues the suspended execution without replaying its reply or completed tools. Pause does not close the current turn or return its pending input to the submission queue. Esc cancels the turn and pauses its goal.

Root and child goals use the same controller. An agent pausing its own goal with `completed` stops that goal's continuation without blocking descendants, so scheduled child work can continue after the parent delivers its answer. User pauses, explicit parent controls, runtime failures, other ancestor pause reasons, and replacement or removal of the parent goal still restrict descendants without changing their own goal states. Resuming an ancestor leaves completed or separately paused children paused. Child goal changes are recorded as revisioned events and delivered through the parent's versioned context records. Child continuation preserves its handle and private context while assigning each execution a distinct run identity. Background execution still requires a live host; this behavior does not detach tasks from session shutdown or restore timers after process exit.

## Persistence and recovery

Managed child agents retain their handle, context, and workspace while paused, including completion pauses, so an explicit Resume can use the same instance. Clear, `flow.kill`, or session shutdown cancels that instance and releases its live resources. A cancelled child cannot be resumed. Process restart restores goal records, not suspended Rust execution or external side effects.

`goal_turn_started` events associate an agent owner and its current goal identity with each turn and run. The goal identity can be absent when the first user turn has not created an objective yet; subsequent `goal_changed` events identify that objective under the same owner.

Root state is stored in `<session>/goal.json`; child records are stored in `<session>/goals/<goal-id>.json`. Writes are atomic and revision checked. Existing `goal.txt` records load paused. Clearing writes a tombstone so legacy text cannot reappear. Opening a session with an interrupted active goal pauses it; recovery requires explicit control and does not replay past side effects.

No default turn, token, fuel, or time budget is added. Provider retries retain their existing limits; exhausted retries pause the goal instead of silently starting another attempt.

## TUI and CLI controls

The root Goal section and child flow panels display padded **Pause / Resume** buttons with `Ctrl+Y`. `:goal` or **Manage Goal** in `Ctrl+P` opens the goal panel. Its agent selector addresses the root or an existing managed child; `Tab` / `Shift+Tab`, `Enter`, and mouse clicks operate the same controls. `Space` toggles the selected goal outside editing. **Save** and `Ctrl+S` submit the objective and return to the goal details after persistence succeeds. A pending save displays **Saving…** and prevents duplicate submissions. Failed saves retain the draft and show the error in the panel. `Esc` discards editing when no save is pending. Clear requires a second activation.

The goal panel opens centered. Drag its title area to move it or its bottom-right handle to resize it. Reopening focuses the existing panel without resetting its position, size, or draft; resizing respects the component's minimum dimensions.

The **Agent** selector chooses the root or a managed child, each with one current objective. **New goal** is always visible; click it or press `N` to open a blank draft. Saving creates a fresh active goal with a new identity and replaces the selected agent's current goal only if its revision still matches. The editor labels this replacement before saving; `Esc` cancels without changing the existing goal. **Edit** changes the current objective while preserving its identity and pause state. New child goals continue to respect parent pauses, and replacing a root goal does not automatically resume descendants attached to the previous goal.

The sidebar Goal card highlights on hover. Click its title, objective, or padding to open the goal panel; drag across the objective to select text. The small left arrow only collapses the preview, and the separate Pause / Resume button only changes execution state. The header displays `:goal` as the keyboard entry point.

Plain REPL commands are `:goal`, `:goal set <text>`, `:goal pause`, `:goal resume`, and `:goal clear`. Controls run immediately instead of entering the prompt queue. Managed `atman run` invocations continue while the goal is active or watchers need handling, and exit on an execution pause or failure, or when neither source has work remaining. Interactive REPL and daemon owners remain available for explicit Resume. No entry point adds a default goal budget.

## Daemon methods

`goal.run` accepts the same parameters as `run_flow` and requires a managed invocation contract with a `user_message` string parameter. It creates an active goal from that parameter and returns `session_id`, the initial `run_id`, and `goal_id`. Subsequent turns have separate run IDs in the event stream. The owner stays live while paused so the authenticated client can resume it. `run_flow` retains its single-execution contract.

`goal.get`, `goal.set`, `goal.pause`, `goal.resume`, and `goal.clear` accept `session_id` and optional `target_handle`. Writes to an existing goal also require `goal_id` and `expected_revision`; `goal.set` requires `text`. Responses contain the same snapshot as the goal tools. Only the authenticated owner of the live session can read or control these goals. `cancel_run` stops the owner and its current turn; clearing removes its goal and stops continuation.
