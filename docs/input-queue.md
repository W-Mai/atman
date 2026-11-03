# Input queue and execution recovery

Sending acknowledges durable acceptance of an input. It does not mean a model has received it or that execution has finished. Each main or floating view sends through its own Flow binding and keeps its own draft, attachments, references, and request acknowledgements.

## Sending and routing

The host classifies the authored prompt before accepting it:

| Input | Handling |
| --- | --- |
| `:recover`, `:cancel`, `:pause`, `:resume` | Host execution controls; these do not become conversation messages. |
| `/command ...` | A captured command entry, arguments, invocation settings, and optional user content, queued for a separate turn. |
| Ordinary text with user routing enabled | The view's configuration resolves `routes.at`; the selected entry and arguments are captured as a command. Even text such as `hi` can take this path. |
| Ordinary text without user routing | A user submission to the retained entry, carrying its text, images, references, and explicit invocation settings. |

Routing uses the requesting view's configuration and project. Quoted text does not select a command or route. Once accepted, the input retains its recorded entry and invocation settings rather than resolving them again when execution begins.

## Waiting and inserting

Normal sends use **next-turn** delivery. An eligible idle Flow admits pending input; an active Flow retains later input in **Inbox** until another turn can begin. Captured entry commands execute separately. Compatible ordinary submissions may be admitted together, while runtime notifications retain their own source and task-scope restrictions.

**Insert** (`i` while Inbox has focus) requests delivery at a compatible model-call boundary in the current turn. It cannot change a request already sent to a provider. A captured command with authenticated user content can be promoted: the shared model context receives its user text, references, and compatible images, while its captured source, arguments, invocation settings, and original receipt remain recorded. If the current turn does not consume it, the retained entry remains available for a later turn. Explicit argument invocations and commands without user content still require a separate turn. Images, invocation settings, and the active turn's phase can prevent insertion. The host rechecks eligibility when processing the action and at the actual pickup boundary.

Ordinary routed messages support the same explicit insertion action as other user submissions. `:insert <text>` explicitly submits plain contextual text without selecting a new routed entry or changing the current invocation settings.

## Execution states

| State | Pending input | Relevant action |
| --- | --- | --- |
| Idle, with scheduling attached and execution allowed | Eligible input can start a turn. | Send or wait for admission. |
| Running or waiting inside a turn | Next-turn input remains queued. A model or tool may still be working. | Wait, request an eligible insertion, or cancel the current turn. |
| Cancelling | Cleanup is still completing; input remains durable. | Wait for the interruption state. |
| Interrupted after cancellation, failure, or host loss | Previously queued input remains stored; it does not automatically restart execution. | **Recover R** or `:recover`. |
| Explicitly paused | The controller's pause prevents execution. | `:resume` releases the local user's pause; other owners' pauses remain. An interrupted Flow still needs recovery. |
| Closing or closed | Closing finishes active cleanup; a closed Flow cannot accept new execution. | Recovery does not reopen a closed Flow. |

Cancelling a turn does not delete its queued messages, close the Flow, undo completed tool effects, or replay the cancelled turn. Input submitted before cancellation remains queued behind the interruption gate. A new authenticated user submission after cancellation expresses new execution intent: it can recover an already interrupted Flow, or allow recovery after cancellation finishes. Runtime notifications do not provide that authorization. Explicit execution pauses remain separate.

## Recovering the Inbox

When an interrupted Flow has pending input, Inbox shows its interruption reason and a three-row **Recover R** button. The button retains blank padding rows above and below its label, with hover and clicks covering all three rows. Its background starts at the Inbox title row without an outer top margin; the title stays to its left and the label aligns with the first status line. Click anywhere in the button, or press `Shift+Tab` to focus Inbox and then `R`. An execution pause, disconnected view, or unacknowledged input disables recovery and displays the blocking condition. Pressing `r` in the normal editor still types a character.

The button submits the existing `:recover` host control through the original view's request channel. It sends no draft text, image, reference, or effort setting. A pending request displays **Recovering** and cannot be submitted twice. A matching failure retains the queue and draft, displays its reason, and permits retry. Replies to a background view remain with that view and cannot clear another view's draft.

Recovery prepares the scheduler before durably releasing the interruption. If preparation fails, the Flow stays interrupted and its pending input remains available for retry. After successful recovery, the scheduler admits eligible pending input in its recorded order; the cancelled turn is not replayed. Execution pauses, task-scope constraints, model availability, and tool failures can still prevent or fail later work. Recovery does not resume an old Goal or remove another controller's restrictions.

## Editing and removing pending input

`Enter` or `e` edits the selected pending user input; **Save** waits for persistence acknowledgement. `Ctrl+↑` and `Ctrl+↓` reorder eligible next-turn user input, and `Delete` removes a pending input. These operations validate the original identity and revision. If another action has already reserved, consumed, or changed the input, the host rejects a stale edit and retains the editor draft. Runtime notifications cannot be edited as user messages. Removing a pending message does not remove committed conversation history or cancel the active turn.
