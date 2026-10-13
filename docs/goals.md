# Managed agent goals

A goal is a persistent objective with `active` or `paused` state. A managed agent completes its current turn normally. An active goal permits another turn; `memory.goal.pause` stops automatic continuation and lets the current turn finish its response. Ordinary custom flows remain single executions unless they declare `contract { invocation { goal: true } }` and run through a goal-aware host.

Managed root and child prompts require every turn to conclude through `final.answer`, including ordinary questions and requests for clarification. The call contains the complete response in `message` and its summary in `_atman_intent`, with no sibling tool calls. When pausing a goal, the agent must receive the pause result before issuing `final.answer` separately; pausing alone does not deliver the final response.

## Goal tools

`memory.goal.get` returns `{goal, execution, paused_by, running_operations}`. `goal` is null when no objective exists. Records include `id`, `revision`, `objective`, `owner`, `parent_goal_id`, `status`, and `pause`.

| Tool | Arguments | Effect |
| --- | --- | --- |
| `memory.goal.get` | Optional `target_handle` | Read the current agent or a descendant |
| `memory.goal.set` | `text`; existing goals require `goal_id`, `expected_revision` | Create an objective or edit its text without resuming it |
| `memory.goal.pause` | `goal_id`, `expected_revision`, `reason`, `summary` | Report completion, blocked work, or missing input and stop continuation |
| `memory.goal.resume` | `goal_id`, `expected_revision` | Resume a descendant, or an agent-paused goal after real user input |

Pause reasons accepted from agents are `completed`, `needs_input`, `blocked`, and `user_requested`. All tools accept an optional `target_handle`. A child cannot address its parent or siblings. Stale revisions fail without changing the goal. A new user request can replace an agent-completed goal with a new identity; editing unfinished work preserves its state. User pauses cannot be overridden by agent tools. Managed agents use pause rather than `memory.goal.clear`.

## Execution boundaries

User Pause stops new requests and tool starts. An in-flight model request finishes and its reply is recorded before execution waits. Already running tools can finish; undispatched work waits. Resume continues the suspended execution without replaying its reply or completed tools. Pause does not close the current turn or return its pending input to the submission queue. Esc cancels the turn and pauses its goal.

Root and child goals use the same controller. Pausing an ancestor adds a restriction to descendants without changing their own states. Resuming an ancestor leaves completed or separately paused children paused. Child goal changes are recorded as revisioned events and delivered through the parent's versioned context records. Child continuation preserves its handle and private context while assigning each execution a distinct run identity.

## Persistence and recovery

Managed child agents retain their handle, context, and workspace while paused, including completion pauses, so an explicit Resume can use the same instance. Clear, `flow.kill`, or session shutdown cancels that instance and releases its live resources. A cancelled child cannot be resumed. Process restart restores goal records, not suspended Rust execution or external side effects.

`goal_turn_started` events associate an agent owner and its current goal identity with each turn and run. The goal identity can be absent when the first user turn has not created an objective yet; subsequent `goal_changed` events identify that objective under the same owner.

Root state is stored in `<session>/goal.json`; child records are stored in `<session>/goals/<goal-id>.json`. Writes are atomic and revision checked. Existing `goal.txt` records load paused. Clearing writes a tombstone so legacy text cannot reappear. Opening a session with an interrupted active goal pauses it; recovery requires explicit control and does not replay past side effects.

No default turn, token, fuel, or time budget is added. Provider retries retain their existing limits; exhausted retries pause the goal instead of silently starting another attempt.

## TUI and CLI controls

The root Goal section and child flow panels display padded **Pause / Resume** buttons with `Ctrl+Y`. `:goal` or **Manage Goal** in `Ctrl+P` opens the goal panel. Its agent selector addresses the root or an existing managed child; `Tab` / `Shift+Tab`, `Enter`, and mouse clicks operate the same controls. `Space` toggles the selected goal outside editing. `Ctrl+S` saves the objective; `Esc` leaves editing. Clear requires a second activation. Saving keeps the draft visible, including when a revision conflict rejects it.

The goal panel opens centered. Drag its title area to move it or its bottom-right handle to resize it. Reopening focuses the existing panel without resetting its position, size, or draft; resizing respects the component's minimum dimensions.

Plain REPL commands are `:goal`, `:goal set <text>`, `:goal pause`, `:goal resume`, and `:goal clear`. Controls run immediately instead of entering the prompt queue. Managed `atman run` invocations continue while active and exit when paused or failed; interactive REPL and daemon owners remain available for explicit Resume. No entry point adds a default goal budget.

## Daemon methods

`goal.run` accepts the same parameters as `run_flow` and requires a managed invocation contract with a `user_message` string parameter. It creates an active goal from that parameter and returns `session_id`, the initial `run_id`, and `goal_id`. Subsequent turns have separate run IDs in the event stream. The owner stays live while paused so the authenticated client can resume it. `run_flow` retains its single-execution contract.

`goal.get`, `goal.set`, `goal.pause`, `goal.resume`, and `goal.clear` accept `session_id` and optional `target_handle`. Writes to an existing goal also require `goal_id` and `expected_revision`; `goal.set` requires `text`. Responses contain the same snapshot as the goal tools. Only the authenticated owner of the live session can read or control these goals. `cancel_run` stops the owner and its current turn; clearing removes its goal and stops continuation.
