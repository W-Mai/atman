# Quick Ask

Quick Ask opens an independent question beside an executable conversation. Click **?** in the composer, choose **Quick Ask** from `Ctrl+P`, or select transcript text and choose **Ask**. The source keeps running and retains its own draft, attachments and controls. Opening a blank question creates no execution owner or saved history.

The first Send captures bounded reference context from the source Flow. Subsequent questions reuse that snapshot and its answers. The question has its own model and reasoning selection; changing them does not change the source. A model switch checks the retained context before accepting a smaller window. Images and registered outputs remain bound to their captured source; omitted or unavailable content is reported in the context detail.

Capture follows the source's selected conversation branch and its checkpoint. Messages from unrelated branch intervals are excluded. An existing question keeps its captured context when the source history changes; refresh remains bound to the original branch.

The question can read and search within its effective authority. It cannot write files, start processes, control other Flows or inherit autonomous Goal continuation. This ceiling also applies when the source uses Eager Allow or Reckless. Its source history is reference data, and the question runs the explicit `quickask.at` policy.

## Actions

Hover a symbol for its explanation. **Question Actions** in `Ctrl+P` exposes the same available actions to the keyboard.

| Control | Behavior |
|---|---|
| **↳** | Focus the source conversation. |
| **↻** in the source strip | Capture newer context from the same source branch while the question is idle. Existing answers remain. |
| **⧉** | Copy the selected answer text or latest answer. |
| **↩** | Append answer text to the source draft as an editable quote, preserving its existing content and attachments. Nothing is sent. |
| **◇** | Keep the idle question as a normal persistent Flow in its Session. |
| **+** | Open a separate new question for the same source. |
| **■** | Stop the question's current response. Retain its conversation. |
| **↻** after a failed handoff | Retry attaching the accepted conversation without sending the question again. |
| **⌫** | Discard a temporary question and release its execution and retained data. |

A source update marks the strip without replacing the snapshot automatically. Changing projects or focused windows does not retarget an existing question. If the source branch changed, returning answer text requires an explicit review of the new target; refresh does not silently switch branches.

## Lifetime and saving

Closing the window only hides it. Reopening Quick Ask for that source reuses its question and native window geometry. Background replies do not move focus. Temporary questions live in their host's memory and disappear when that host exits; they do not appear as saved Flow history before Keep.

Keep preserves the conversation, retained assets and pending input, then attaches the durable Flow to the same window. It creates no new model request. Accepted pending input remains held until a new user submission releases scheduling, including after restart. An uncertain save retains readable temporary data and retries the same operation; it does not create another saved copy. Saved questions appear under **Activity → Flows** and use normal Flow history and recovery.

## Model caching

Question calls request provider caching automatically. Subsequent questions can reuse their stable prefix, but a first question may have a cold cache because its policy, tools and reference projection differ from the source. Explicit source refresh and model changes can change that prefix. Usage displays provider-reported cache reads; retaining a local snapshot is not itself a model-cache hit.
