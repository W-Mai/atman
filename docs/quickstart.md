# atman quickstart

From zero to a running code agent. Reads top-to-bottom in ~10 minutes.

## 1. Install

Prereqs: Rust 1.85+, git, and (optional but recommended) an Anthropic or OpenAI API key.

For a released binary on macOS or Linux:

```bash
curl -fsSL https://atman.run/install.sh | sh
```

Other installation options:

```bash
brew install W-Mai/cellar/atman-cli
cargo install atman-cli --locked
```

For a local checkout:

```bash
git clone <your atman checkout url> ~/src/atman
cd ~/src/atman
cargo install --path crates/atman-cli
```

The installers place `atman` in a platform-specific executable directory. For Cargo-managed installs, make sure `~/.cargo/bin` is on your `$PATH`.

Verify:

```bash
atman version
```

To update an installation managed by the official installer:

```bash
atman upgrade
```

`atman upgrade` does not update a Homebrew Cellar. Use `brew upgrade atman-cli` for Homebrew-managed installs.

## 2. Scaffold your config

```bash
atman init
```

This writes:

```
~/.config/atman/
├── config.toml                # all sections optional, defaults are fine
├── on_session_start.at        # REPL greeting flow
├── routes.at                  # bare-text → slash-command routing
├── prompts/
│   └── system.md             # managed system prompt
└── commands/
    ├── agent.at               # canonical code-agent loop
    └── hello.at               # smoke-test flow
```

`atman init` is idempotent: re-running never overwrites files you have edited. Only missing files get filled in from templates.

## 3. Set an API key

Pick one provider, export the matching env var in your shell rc:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
# or
export OPENAI_API_KEY="sk-..."
```

Optionally point at a compat gateway with `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`.

OpenAI-compatible connections select the API separately from the Chat reasoning parameter format:

```toml
[providers.gateway]
kind = "openai-compat"
base_url = "https://gateway.example/v1"
api = "chat-completions"
reasoning_format = "reasoning-effort"
# Enable only when the gateway accepts OpenAI's prompt_cache_key field.
# prompt_cache_key = true

[models.reasoning]
provider = "gateway"
model = "vendor/reasoning-model"
api = "responses"
context_budget = 128000
reasoning = "high"
```

The model's `api` overrides the provider default; omit it to inherit. Existing OpenAI-compatible configurations without `api` use Chat Completions. Responses supports API-key authentication and preserves model IDs such as `vendor/reasoning-model`. `reasoning_format` applies only to Chat Completions. The provider manager's API selector and model API override save these same fields.

Use a connection root in `base_url`; do not append `/responses` or `/chat/completions`. **Test connection** checks the provider's model discovery endpoint. **Test model** sends a small model request using the draft protocol, reasoning selection, and tool schema without running an external tool.

Provider Manager supports local Search (`/`), padded records, and a details view. Click a record or press `Enter` to view its details. Config connections expose Edit (`e`), Models (`m`), Test (`t`), Enable / Disable (`d`), and Remove (`Del`); OAuth connections expose Models, Enable / Disable, Refresh (`r`), and Remove. Environment entries are read-only. Add (`n`) retains the supported presets and Custom option. Narrow terminals separate the list and details and place additional actions under More.

Provider forms use `Tab` / `Shift+Tab` between fields and buttons, arrow keys for selectors, and mouse clicks for cursor placement. Save or `Ctrl+S` commits the draft; Cancel or `Esc` leaves it, asking Save / Discard / Keep editing when necessary. Hide during a pending operation preserves its state. Connection test results remain available in Details and belong to the tested draft. Removing a connection requires confirmation; referenced configuration models must be updated first.

Model Manager opens either All providers or a single provider scope. Models are grouped under provider display names and model counts; empty providers remain visible. Search (`/`) filters within that scope by model or provider name and retains the matching groups. `PgUp` / `PgDn` uses the padded group and model geometry, with the provider heading repeated on continuation pages. Arrow keys and the mouse wheel also reach empty provider pages. `Enter` or a model record click opens details. Configured entries support Edit (`e`) and Delete (`Del`); Catalog entries are read-only. Add (`n`) creates a configuration, and Create alias (`a`) opens the alias form. Cached catalogs for disabled providers remain available for inspection without enabling those providers. Narrow terminals expose additional actions through More.

Model forms use `Tab` / `Shift+Tab` between fields, Details, and action buttons. Click a text value to position the cursor; selector arrows support keyboard and mouse. Save or `Ctrl+S` commits, while Cancel or `Esc` returns with Save / Discard / Keep editing for unsaved changes. Pending operations can be hidden and reopened with their drafts and results intact. Only Stop test cancels the model test. Open Details to read wrapped diagnostics with arrow keys, `PgUp` / `PgDn`, or the mouse wheel. Delete requires confirmation and displays blocking aliases with an entry to manage them. These operations do not switch the current session model.

Alias forms retain Provider groups and model counts. Press `Tab` from the name to the model list, then `/` or click Search to filter by model name, API ID, or provider name. `Tab`, `Enter`, or `Esc` from Search returns to the model list; `Ctrl+U` or the clear button clears the query. Use `←` / `→` or the Group buttons to cycle provider scopes, `↑` / `↓` or a model click to select, and `PgUp` / `PgDn` to page. Search and scope changes keep the bound target unchanged. Save or `Ctrl+S` commits that target; `Enter` in the model list explicitly selects its focused model before saving. Small terminals keep padded model records and action buttons; insufficient space displays a size prompt.

The alias manager provides New, Edit, and Delete buttons. In an alias form, `Tab` / `Shift+Tab` switches between the name and model list; click the name to position the cursor or click a model to select it. Arrow keys, the mouse wheel, and `PgUp` / `PgDn` navigate the list. Editing preserves the saved target, including a target absent from the current catalog, until another model is selected. Save, `Enter`, or `Esc` saves a valid form.

Choose **Switch Model** from `Ctrl+P`. Use arrows, the mouse wheel, `PgUp` / `PgDn`, or `Home` / `End` to navigate; click a model or press `Enter` to switch. The picker stays open until the switch succeeds. Initial setup also provides clickable provider and model controls: select a model, then click Finish or press `Enter`; `Tab` / `Shift+Tab` moves between the list and actions. Both panels keep padded records and visible controls in narrow terminals.

The default OpenAI API endpoint and Codex enable stable prompt-cache routing by default. Custom OpenAI base URLs and OpenAI-compatible gateways leave it disabled unless `prompt_cache_key = true` is configured explicitly. Changing the API changes the cache projection; provider usage reports remain authoritative for cache hits.

Set `max_tools = 128` in a provider table when that endpoint limits the total number of request tool definitions to 128. The count includes built-in tools and native MCP tools. The official OpenAI Chat Completions endpoint applies this limit by default; other endpoints require an explicit limit. A request exceeding the limit fails locally with the actual count, before sending its conversation history. MCP broker calls do not add target definitions to the request.

Model reasoning and image defaults can be configured per model:

```toml
[models.smart]
provider = "anthropic"
model = "claude-opus-4-6"
context_budget = 200000
reasoning = "high"
input_modalities = ["text", "image"]
image_detail = "auto"
```

Reasoning can also be selected for one run with `--reasoning high`; repeat `--image path/to/image.png` to attach images. In the TUI, Cmd+V, Ctrl+V, or Alt+V attaches the clipboard image. Its `[image N]` reference and attachment bar remain visible until submit; deleting the reference removes that image. Ctrl+U removes the pending quote first, then the latest pending image. The input border shows the effective reasoning depth for the next submission, and Ctrl+T cycles that input value. It is exposed to the flow as invocation-local `env("effort")` data. `llm.call`, `llm.extract`, `llm.classify`, and `llm.generate_branches` consume it only when their own call explicitly passes `effort: env("effort")`; inheriting the invocation environment alone does not change a request.

`llm.call(retry: N)` retries transient network failures (including SSE response-body transport failures), timeouts, rate limits, provider outages, and empty assistant replies. Empty replies do not enter session history. Authentication, invalid requests, and unsupported tool schemas fail immediately; `retry_classified: [...]` explicitly selects other error categories when a flow requires them. Context-overflow recovery still compacts managed input before resending. Failed calls using managed context release the context lock before automatic compaction; calls using `context: "none"` or explicit `messages` do not trigger compaction of the session history. Requests never switch API protocol or reasoning effort in response to a protocol rejection.

Drag across transcript text, line-end space, or short Markdown gaps to open Copy, Rich Copy, Quote, and Cancel actions; holding the pointer past the visible document edge scrolls the selection. Quote appears above the input until submission, separate from the editable prompt. Click its card or press F4 to expand it, then use PgUp/PgDn or the mouse wheel to scroll; click `[x]` or press Ctrl+U to remove it. A short terminal shows the pending quote count in the input border.

Models with image input enabled can also call `image.read(path: "...")` to inspect a local PNG, JPEG, GIF, or WebP file during an agent run. The tool imports at most 20 MiB through the session attachment store and supplies the image to the next model call without placing base64 in the textual tool result.

Persistent main and floating views keep separate image drafts, even when displaying the same flow. A successful submission clears only its originating, unchanged draft; failures and later edits retain the images. Opening a panel or sending a control command keeps images that were not included in that command.

Colon controls such as `:pause` and `:status` submit only the command. Pending quotes and images remain in the composer for the next message; quoted text cannot select a control command. Ordinary prompts and `/command` flow calls include their quotes and images.

Use `@./image.png`, `@../image.png`, or `@/absolute/path.png` in a prompt to attach a local image. Persistent terminal, main, and floating views resolve relative paths from the selected flow's workspace or bound project. The image is copied into that flow's storage before input acceptance, so deleting the original file does not break accepted input. File references inside quoted context remain quoted text.

Use `:attach <path>` to add an image before composing a message, including paths containing spaces. `:attach list` shows the current view's images and `:attach clear` removes them from its draft. Imports use the same flow directory and storage as inline images. Switching flow focus preserves each open view's pending attachments. A failed import or rejected input keeps existing attachments; only an accepted message consumes the images it submitted. Clearing a draft does not delete files referenced by accepted messages.

Optional tool output payload limits can be set in `~/.config/atman/config.toml`:

```toml
[tool_output]
max_lines = 32
max_bytes = 1024
max_line_bytes = 384
```

Tool dispatches appear as ordered `working · N` blocks. Click an intent once for a bounded tail or edit-hunk preview and again for the complete output; Bash, Terminal, sub-agent, and diff details retain their fullscreen action. When a managed agent begins its final response, preceding thinking, tools, and workflow activity fold into a five-row `work` summary containing completed progress, edit metrics, and an activity description; click the summary or press `Alt+O` to reverse the 400–700 ms animation without resetting nested expansion state. File edit totals update as successful writes are applied. Diff views default to two columns and can be changed with `[diff] layout = "unified"` in `config.toml`.

`fs.read` exposes `start_byte_in_line` for continuing a long UTF-8 line; `bash.output` continues with `next_cursor`, and `term.capture` continues with its returned terminal-area coordinates.

## 4. Sanity check

```bash
atman doctor
```

You should see:

- Your `data_dir` and `config_dir` (both auto-created if missing).
- One row per provider: `[✓]` if the env var is set, `[✗]` if not, plus `reachable (HTTP …)` / `unreachable: …` for the base URL.
- Local preview status (`preview.push` starts Atman's preview service when needed).
- Project and user rules and skills picked up from CLAUDE.md, `.agents/skills`, `.codex/skills`, `.claude/skills`, `.github/skills`, `.cursor/skills`, `.kiro/skills`, `.vscode/skills`, and the user-level Claude skills directory.

If a provider row shows `unreachable`, fix that before moving on.

## 5. First REPL turn

```bash
atman
```

You'll land in an interactive prompt:

```
atman v1.8.0 — type `:help` for commands, `:exit` to leave
[atman] session=… events=/…/events.jsonl
atman ready. `/hello` for a smoke test, plain text to chat.
atman>
```

Three input modes:

- `:name`     — REPL builtin (`:help`, `:exit`, `:cost`, `:goal`, `:suggest`, …).
- `/name arg` — run `<project>/.atman/commands/<name>.at`, falling back to `~/.config/atman/commands/<name>.at`; a project command with the same name takes precedence.
- plain text — `routes.at` handles configured prefixes and its default route.

`:suggest` examines up to 30 recent turns in the current flow's message snapshot, including tool names, arguments, and shortened results. Watcher and child-result messages retain their source labels; internal control messages, system context, and private reasoning are excluded. It does not scan event files or merge other flows. A proposal must pass flow validation before confirmation, and acceptance installs it in the session's configuration directory without executing it.

Persistent terminal, main, and floating views run suggestions asynchronously. One suggestion may be pending per flow; other flow controls remain available. Confirmation belongs to the requesting flow and expires after five minutes without installing anything. The plain terminal uses `:reply <prompt-id> {"Submitted":{"answers":[{"kind":"confirmed","value":true}]}}` to accept or `:reply <prompt-id> "Rejected"` to discard. Switching views does not redirect the result, and detaching the terminal cancels suggestions still awaiting generation or confirmation. Suggestion errors do not fail or resume a flow.

`:compact` and the TUI's Compact Transcript action use the same explicit context operation. Persistent views keep the operation attached to its original flow while allowing navigation and other controls; repeated requests for the same flow are rejected until the operation finishes. TUI review uses `[compaction].review`, and the result distinguishes a committed compression, no smaller window, rejection, or failure. A paused flow stays paused, and full conversation history remains available. The summary uses the flow's latest model, or its configured `smart` alias before any model call.

Persistent execution views accept `:copy last-message`, `:copy last-tool`, and `:sidebar on|off`. Copying reads the bound flow's original records rather than its compacted model window. Plain terminals receive an OSC 52 clipboard sequence; TUI requests use the existing clipboard handler and display the result in the requesting view. These commands do not submit user input or resume execution.

The TUI's `:storage` and `:formula` selectors use the configuration bound to the current view. Storage changes apply to new sessions in that project; formula display changes take effect after restart. Settings stay open until saving succeeds, and failures retain the selected value. These dialogs do not add runtime prompts or conversation records; runtime questions received while a settings dialog is open appear after it closes.

When a completion menu is open, use `↑`/`↓` to select an item and `Enter` or `Tab` to place it in the input. Press `Enter` again to submit the completed command.

In the TUI, `:panel` opens the `Ctrl+P` command palette without requiring the shortcut. Palette actions also have direct commands, displayed beside each menu item:

| Action | Command |
|--------|---------|
| Command palette | `:panel` |
| Provider, model, and alias managers | `:providers`, `:models`, `:aliases` |
| Switch active model | `:model` |
| MCP servers; Memory & Rules | `:mcp`, `:memory` |
| History search; keyboard shortcuts | `:history`, `:keybindings` |
| All Projects; project storage scope | `:projects`, `:storage` |
| Trust mode; display theme; formula rendering | `:mode`, `:mode-theme`, `:formula` |
| Switch; create; move session | `:sessions`, `:new-session`, `:move-session` |
| Open the profile's Atman Session | `:atman` |
| Choose a session to delete; generate a name | `:delete-session`, `:name-session` |
| Toggle sidebar; select transcript text | `:sidebar`, `:yank` |
| Copy last assistant message or tool result | `:copy last-message`, `:copy last-tool` |
| Compact transcript | `:compact` |

Panel commands use the same actions as the palette and remain available during a running flow. Opening a panel does not submit a prompt or consume pending quotes or attachments. Floating windows support title-area dragging and bottom-right resizing; reopening preserves their position and dimensions. Narrow MCP and Knowledge windows initially use the available canvas and retain subsequent manual adjustments. A close action blocked by an unfinished edit displays a reason. Transcript commands such as `:copy` and `:compact`, and commands with arguments such as `:storage local` and `:sidebar off`, retain their existing behavior. `:help` prints command help; `:keybindings` opens the shortcuts panel. The plain REPL completes commands from the same registry and reports when a command requires the TUI.

Tool-call purposes become the primary labels in output blocks, workflow nodes, approval rows, sub-agent panels, task windows, and the Tasks activity area. The tool name and source handle remain visible as secondary technical metadata. Expanded Bash and Terminal output blocks and their floating task panels also show the original spawn command for auditing. The managed agent writes these dynamic labels in the current user's language when practical. The Tasks activity area shows running leaves, so a dispatcher is hidden while its concrete parallel tool calls are active.

Main and floating flow views use the same tool-output expansion rules, detail targets, and workflow replay conversion. Replayed node timestamps, model usage, and tool results retain their recorded values in either view. Each view retains its own expansion state. Floating message updates preserve tool expansion by call identity; resizing or expanding an item does not rebuild its source messages.

Final replies in floating flow windows fold their preceding message activity into the same `work` summary used by the main view. Click the summary, or focus it with `Tab` and press `Enter`, to expand or collapse it. Message updates preserve that choice for the same final reply. Fold state belongs to each view, so expanding a floating window does not change another view.

Click a Flow task's sidebar cross once to stop it. The task window’s close button also stops the flow. Pressing `Esc` only closes the floating window and leaves the task running. Bash and Terminal crosses retain two-click confirmation. A stopping task stays visible until execution exits, then moves out of the active list while its details remain available in History.

Child flows can remain available after delivering a final answer. Their task cards show execution activity separately from handle lifetime: paused or idle children use a static waiting indicator, and running children animate. Waiting does not mark an unfinished child complete or prevent later resumption. Child task panels retain restored tool output across expansion, scrolling, and resizing; new content refreshes the projection without resetting tool disclosure. Click a working group, tool header, or output preview to expand it. Inside a focused Flow panel, Tab / Shift+Tab selects visible controls and Enter or Space activates them; fullscreen actions open the selected task or output.

`Ctrl+Tab` / `Ctrl+Shift+Tab` cycles floating-window focus in creation order on terminals that report those modified keys. Theme and trust-mode pickers accept arrow keys, wheel navigation, and clicks anywhere in a padded option; `Enter` applies the selection, and **Cancel** or `Esc` closes the picker.

Try the smoke test first:

```
atman> /hello
hello from atman
```

The bundled command explicitly delivers its reply and returns the same text:

```at
flow hello() -> string {
    return final.answer(message: "hello from atman")
}
```

`final.answer` publishes one reply in the calling context. A plain `return` only passes a value to its caller; an interactive entry point can display that value when the flow has not published a reply. Calling a flow from another flow does not automatically display its return value.

Then the code agent:

```
atman> list the .at files under examples/ and pick one to summarise
[agent loops, calls fs.list, reads files, replies …]
```

While a flow is running you can:

- Type a normal message to place it in the ordered `next` queue without interrupting the current flow. The next task-facing `llm.call` consumes eligible queued messages as trailing user context; if the flow makes no further call, they run as new turns afterward. Commands, path attachments, and submissions with an explicit reasoning setting retain their new-turn behavior. Press `Shift+Tab` to focus the queue, use `↑`/`↓` to select, `Ctrl+↑`/`Ctrl+↓` to reorder, `Enter` or `e` to edit, `i` to insert the selected text task into the current flow's next LLM call, and `Delete` or `Backspace` to remove. The same actions are available with the mouse. Insertion waits in L1 without adding a transcript entry; if the turn ends before pickup, the task returns to Next.
- `!nudge <text>` — L1 nudge (added to context at the next LLM node boundary).
- `!course-correct <text>` — L2 (mid-stream restart with the correction).
- `!redirect <flow>` — L3 (switch to another flow).
- `!stop` — L4 (kill immediately).

A `form.ask` uses `↑`/`↓` to select choices and `Tab`/`Shift+Tab` to move to the next or previous question. A form that times out can still be answered while it remains open. The answer is recorded with the original question and added to the next task-facing `llm.call` as user context; it does not start or interrupt a flow by itself.

Form choices are clickable across their padded rows, including wrapped options. Clicking a multi-select option toggles it; clicking a text answer positions the cursor. Use the mouse wheel to scroll long questions. Review the final Yes/No confirmation before submitting all answers.

Compaction review provides Accept / `Enter`, Edit / `e`, and Reject / `Esc` buttons. While editing, Save / `Enter` accepts the edited summary; Back / `Esc` returns to viewing and keeps the edits for later acceptance. Click the summary to place the editing cursor, use `Shift+Enter` for a new line, and scroll either pane with the mouse wheel. In viewing mode, `↑` / `↓` scrolls the summary and `PgUp` / `PgDn` scrolls the source slice; in editing mode, `PgUp` / `PgDn` scrolls the summary.

Press `Ctrl+L`, choose **All Projects** from `Ctrl+P`, or run `:projects` to open Project Hub. Use the arrows to select a project, `Enter` to inspect its sessions, `P` to pin or unpin it, and `X` twice to archive it. An archived project can be restored with `X` or deleted with `Delete`/`Backspace` twice. Delete removes Atman-owned sessions, goals, plans, tasks, confessions, specs, previews, and indexes while preserving the source directory, Git repository, and hand-written `.atman` configuration, commands, skills, flows, and rules. The same actions are clickable. A missing project path still allows stored sessions to open; workspace tools report the unavailable path when used.

Project Hub loads its directory asynchronously. Use **Refresh** or `R` to update it; refresh and project actions preserve the selected record and detail page. Errors stay in the panel for retry. In a persistent session, Project Hub includes persistent sessions and original legacy histories. Opening a Session row shows its flows in FlowHub; choose a flow for the main workspace or a floating window. Original histories open as read-only snapshots without migration or execution services. Drafts remain with their original views. Legacy execution attachments retain their existing session switcher.

Command Palette and Search History keep long queries in a horizontal input viewport. Click the query to position its cursor. In Search History, click a result to update its preview; the mouse wheel navigates results or scrolls the preview according to the pointer's location. The query field remains visible in small terminals.

**Manage Memory & Rules** in `Ctrl+P` uses padded records with clickable actions and visible shortcuts. In a narrow window, click `›` or press `Enter` to expand details; arrows, `PgUp` / `PgDn`, and the wheel scroll the detail view. Click `‹ List` or press `Esc` to return. Editing keeps the focused field visible: `Tab` / `Shift+Tab` changes fields, `Shift+Enter` inserts a newline, `Enter` saves, and `Esc` cancels.

Choose **Set Project Storage Scope** from `Ctrl+P` or run `:storage` to select where new sessions for the current project store Atman data. `global` uses the central Atman data directory; `local` uses `<project>/.atman`. `:storage global` and `:storage local` apply the choice directly. The setting is saved in the project's `.atman/config.toml` and takes effect for new sessions.

## 6. Control a persistent goal

The default agent creates a goal for actionable requests. After a turn finishes, an active goal starts another turn until the agent reports completion, needs input, encounters a blocker, or receives a pause. Ordinary questions do not require a goal. The objective remains explicit when older conversation ranges are compacted.

```
atman> :goal set finish the requested changes
atman> :goal pause
atman> :goal resume
atman> :goal clear
```

In the TUI, click **Pause / Resume** beside the goal or press `Ctrl+Y`. `:goal` and **Manage Goal** in `Ctrl+P` open the bound flow's goal details, editing, and controls. Select another flow through FlowHub or open its floating view to manage its objective. `Tab` / `Shift+Tab` selects controls, `Enter` activates them, and `Ctrl+S` saves an edited objective. Mouse controls use the same actions.

Goal Pause stops objective continuation while allowing the current turn to finish. It does not block incoming work or pause another flow. Goal Resume queues a new turn; execution pauses require separate execution controls. Clearing removes the objective without cancelling execution. Changing its text does not resume it. `Esc` closes a floating view or navigates out of the focused view; stopping work requires an explicit execution action.

Goals are stored in the owning flow's journal with an identity, revision, and state. Recovery preserves the objective without replaying interrupted execution. Older session formats remain available as read-only history or explicit archives. See [Goal controls](./goals.md) for execution semantics and daemon methods, [Storage](./storage.md) for persistence, and [Context strategy](./context-strategy.md) for request composition.

Use the goal for the objective, `plan.write` / `plan.tick` for the high-level ordered route, and `memory.todo.*` for concrete execution items inside the current plan step. Goal and active plan are reassembled into the system context; todos remain available through tools and the UI.

## 7. First flow snapshot / test

`atman run file.at --flow name key=value` records execution in a session containing persistent flow instances. `atman session list`, `atman session show <session-id>`, `atman logs tail <session-id>`, and `atman cost <session-id>` inspect the recorded run. `--ephemeral` discards the temporary session after execution and notification delivery finish. `--reasoning high` supplies an invocation value; a model call uses it only when the flow explicitly passes `effort: env("effort")`.

atman ships two authoring conveniences worth trying early.

**Snapshot your flows in a versioned registry** (opt-in):

```bash
export ATMAN_AUTO_SNAPSHOT=1
atman run ~/.config/atman/commands/hello.at
atman flow versions hello
```

Snapshots live in `<project>/.atman/flow-registry.db`. When you edit a flow and it starts misbehaving:

```bash
atman flow diff hello <old-hash> <new-hash>
atman flow rollback hello <old-hash>              # writes back the source
```

A snapshot of a flow with `use` records its reachable `.at` sources. Editing a used source creates a new revision, and `atman flow diff <name> <old-hash> <new-hash>` shows changes in each source file. Run recorded code with `atman flow run <name> --revision <id-or-hash>`; the run uses current tools, configuration, and filesystem effects. `flow rollback` refuses revisions with dependencies because replacing only the entry file would leave an inconsistent source tree.

**Regression-test a flow** (offline; uses a mock provider):

```bash
atman flow test ~/.config/atman/commands/hello.at
```

First run writes `hello.at.snap.json`. Subsequent runs compare the current output to the snapshot; mismatches print one line per drift case and exit non-zero. Re-run with `--bless` when the change is intended.

### Reuse a flow from another file

Put a shared flow in `<project>/.atman/lib/text.at`:

```atman
pub flow normalize(input: string) -> string {
    return input
}
```

Call it from `<project>/.atman/commands/review.at`:

```atman
use "project:text.at"::normalize

flow review(input: string) -> string {
    return normalize(input).await
}
```

Run the entry flow with `atman run .atman/commands/review.at --flow review input=hello`. A relative specifier such as `use "./lib/text.at"::normalize` resolves beside its declaring file. `use "user:text.at"::normalize` reads from `~/.config/atman/lib/`. A named `use` makes only the selected public flow callable in the current file; the shared file's private helper flows remain available to its own flows. Calling `normalize(input)` creates a cold Future; `.await` executes it, and `fanout [normalize(a), normalize(b)]` runs both calls concurrently.

An absent project or user source directory does not block unrelated sources. An import that requires the missing directory still fails; existing roots retain their path and symlink boundaries.

### MCP discovery and calls

The MCP browser supports arrows to select a server, `Enter` or a click to expand it, and `Tab` to cycle Tools, Resources, and Prompts. Scroll expanded descriptions or diagnostics with the wheel or `PgUp` / `PgDn`. Add / `a`, Edit / `e`, Test / `t`, Toggle / `d`, and Remove / `r` have clickable buttons. Remove first displays Confirm / `Enter` and Cancel / `Esc`; changing the selection cancels the pending removal. Small terminals retain a complete server record above the actions.

The MCP Add/Edit form uses `Tab` / `Shift+Tab` or vertical arrows to switch fields. Click a text value to position the cursor; click selector arrows to change transport or approval tier. Save / `Enter` applies the draft, while Cancel / `Esc` closes without saving. Environment entries use one `KEY=value` per line; `Shift+Enter` adds a line. Scroll long validation details with the mouse wheel over the message, or `PgUp` / `PgDn`.

The managed agent exposes `mcp.status`, `mcp.tools`, `mcp.await`, and `mcp.call`. The request's MCP definitions stay fixed as the catalog grows. Tool descriptions and schemas are retrieved as tool results, so discovery does not replace the model's existing tool prefix.

Calls to tool names absent from the current request return `tool_not_exposed` without executing the tool. For MCP names retained in conversation history, the error provides discovery and broker guidance only when the originating request exposes `mcp.tools` and `mcp.call`. A unique permitted catalog match includes the exact `server` and `tool` for schema retrieval; unresolved names require searching the permitted catalog. The model must issue a new call, which still passes target permissions and approval. Ambiguous call IDs and missing or consumed authorization receipts have separate errors and do not grant a recovery target.

`mcp.tools(query: "search", limit: 20)` returns `{tools, next_cursor, total}`. Each catalog item includes `name`, `server`, `tool`, and `description`. Continue with the returned `next_cursor` and the same filters; a null cursor marks the end. Results are sorted by qualified name. `limit` accepts 1–100 and defaults to 20. Query terms match tool names and descriptions, ignoring case. Set both `server` and `tool` to retrieve an exact tool, including its complete `input_schema`, before calling it:

```at
ready = mcp.await(server: "jira")
catalog = mcp.tools(server: "jira", query: "search")
definition = mcp.tools(server: "jira", tool: "search")
result = mcp.call(server: "jira", tool: "search", input: {query: "open"})
```

`mcp.call` uses the real target's approval level, permissions, scheduling resources, and audit identity. The model's call ID stays unchanged. Model tool batches resolve only registered targets; a call to an unavailable tool returns an error immediately, allowing other tools in the batch to run. Use `mcp.await` before calling a server that is still connecting. Direct `.at` calls can wait for that server. The MCP adapter validates arguments against the original JSON Schema after argument reconciliation and before transport. Local schema references are supported; external HTTP and file references are rejected. An invalid schema disables calls to that tool without preventing unrelated tools from registering.

Custom `llm.call` flows can expose native tools with a named selector such as `"mcp.jira.search"` or `"mcp.jira.*"`; those selectors wait for the server and fail if it is unavailable. The broad `"mcp.*"` selector gives connecting servers a short discovery window and exposes available native tools without failing because another server is unavailable. Native definitions count toward the provider's tool limit. Exposing the broker controls alone grants MCP discovery and calls across the catalog, subject to target permissions. When the same request also includes specific MCP selectors, those selectors restrict discovery, status queries, readiness waits, and calls. Status counts include only permitted tools. Waiting for one server does not block unrelated tools in the same batch. Exposing `mcp.call` never bypasses the target's approval policy.

The CLI supports the same direct operation with JSON input. Tools declaring the MCP `readOnlyHint` annotation can run directly; tools without that declaration require the explicit write override:

```bash
atman mcp call jira search '{"query":"open"}'
atman mcp call jira update_issue '{"key":"PROJ-1","summary":"Updated"}' --allow-write
```

#### Connection lifetime

`AtmanRuntime::build` owns configured MCP connections for CLI, daemon, and embedded execution. Executor clones and their flows share that service; session views only subscribe to its status. `executor.tool_ctx.mcp_service()` provides status subscriptions, configuration reload, and an async shutdown that waits for connection cleanup. Reload removes old adapters and cancels connections still initializing or refreshing; results from a superseded configuration cannot restore them. Empty or disabled-only configurations do not start a worker thread. A server failure remains visible in `mcp.status`; only calls that explicitly require that server fail.

For embedded hosts, `McpClient::connect_config_in(config, Some(absolute_directory))` fixes a stdio server's working directory, including reconnects. Project-backed CLI startup supplies the runtime's bound project root. `connect_config` captures the process working directory when connecting. `McpClient::shutdown()` closes the connection, cancels pending requests and sampling tasks, and prevents subsequent calls or catalog refreshes from reconnecting. Dropping the final client releases its transport; a failed or abandoned initialization also releases its child process. Stdio stderr is drained without being rendered into the terminal UI.

#### HTTP MCP configuration

Configure an HTTP endpoint in `~/.config/atman/mcp_servers.json`:

```json
{
  "mcpServers": {
    "remote": {
      "type": "http",
      "url": "https://mcp.example/mcp",
      "authToken": "YOUR_TOKEN",
      "headers": {
        "X-Workspace": "example"
      },
      "timeout_ms": 30000
    }
  }
}
```

HTTP requests always advertise `Accept: application/json, text/event-stream`; additional configured media types are retained. Custom headers apply to initialization, notifications, calls, and reconnection through the CLI, TUI, and daemon. If both `authToken` and an `Authorization` header are configured, `authToken` takes precedence. Atman manages `Content-Type`, `MCP-Protocol-Version`, and `Mcp-Session-Id`; configured values cannot override them. Redirects are rejected, so use the endpoint's final URL.

The HTTP handshake requests protocol version `2025-11-25` and accepts the server's supported `2025-11-25`, `2025-06-18`, or `2025-03-26` revision. Responses may contain JSON or SSE; SSE supports batch messages, notifications, and server ping requests. A matching result is delivered without waiting for the server to close the connection. An HTTP 404 for an established session triggers one reinitialization and retry. Timeouts, network failures, and other HTTP errors are returned without automatically replaying a request that might already have executed. The `sse` configuration type uses the same HTTP POST transport; legacy GET/endpoint SSE negotiation and background GET notification streams are not supported.

## 8. Session discovery and spec state

`atman session new` creates a persistent session and default flow for the current project, then prints its UUID. Creation does not initialize providers or MCP servers, run startup scripts, or call a model. Open it with `atman --continue <uuid>`; an optional `on_session_start.at` captured during creation runs on this first attachment. Without that script, the flow waits for input. Add `ATMAN_NO_TUI=1` for a plain terminal.

Launching `atman` without a subcommand now creates and attaches a project-bound persistent Session through the same host used for `--continue`. The welcome view includes recent sessions and an All Projects entry; the active Flow remains available after the terminal detaches and can be reopened by UUID.

Persistent TUI attachment displays runtime initialization progress, carries startup notices into the view, and opens setup when the session's configuration lacks credentials or a selected model. Startup forms use the same form controls as subsequent turns. Leaving the interface releases the terminal after host shutdown; persistent flows remain available for another attachment.

For a persistent session created by `atman run`, use `ATMAN_NO_TUI=1 atman --continue <session-id-or-prefix>` to attach a plain terminal. `:flows` lists its instances and `:focus <flow-prefix>` selects the recipient of subsequent input without stopping other flows. `:session`, `:sessions`, `:model`, and `:cost` inspect the attached session and flow; `:storage` reads or changes the project storage scope. Text uses the entry's declared `invocation { user_message: parameter }` mapping; `:invoke {"parameter":"value"}` supplies explicit arguments. Use `:help` for execution controls, Goal controls, prompts, and approvals. `:quit` detaches; EOF waits for runnable work. The session retains its source snapshot even if the original `.at` file is removed.

In a persistent TUI, choose **New Session** from `Ctrl+P` or use `:new-session`. The new session uses the selected view's project and configuration, with an independent transcript and Goal. `on_session_start.at`, when present, runs once through the new flow; scripts publish visible replies with `final.answer`. Normal text uses that configuration's `routes.at`, and `/name args` invokes a command directly. Switching to the new session preserves the original view's draft and background work. Closing the creation window prevents automatic navigation without cancelling an accepted creation; the new session remains in FlowHub.

Open `:flows` to browse a Session's instances. The directory starts in Graph mode; **List / Graph** or `Ctrl+G` switches layouts while retaining selection and filtering. Use ↑/↓ to select, Enter or a node click to focus, and **Window** or `Ctrl+O` to open a floating view. The selected node shows directed relation links. Type a title, ID, status, or `link:watch` to filter the current page; **Links** or `Ctrl+Shift+G` opens the relation directory. PgUp/PgDn changes pages, and **Refresh** or `Ctrl+R` reads a new snapshot without rearranging existing nodes on that page.

The top navigation opens **FlowHub**, **WatchHub**, and **Worktrees**; `Ctrl+Tab` and `Ctrl+Shift+Tab` cycle between them. The Workspace rail contains the Atman management entry, the cross-Session overview, the complete Session directory, and Session scopes collected from inspected pages. `F2` focuses this rail; on narrow terminals it opens a separate navigation page. Use ↑/↓ and Enter, or click a padded row. Esc returns from the navigation page without closing the directory. Main and floating views use the same navigation and preserve the conversation's draft.

`:sessions` opens Switch Session. Find/F searches, Scope/Tab switches project scope, and Sort/S orders conversations by recorded activity or message count. Enter or a row click opens the default Flow directly and retains drafts in existing views. Rename/R and Save/Ctrl+S edit a title; Delete/D opens deletion review. Each action also has a mouse button.

In `:flows`, choose **All flows** or press `Ctrl+B` for the cross-Session overview. Session bands group the loaded nodes; graph and list share search, selection, Focus, and floating views. Cards show acknowledged or detached execution, pending input, the current Goal, and the default-view marker. PgDn loads another bounded batch after the visible matches; a search with no matches can still load more. Counts distinguish loaded Flows from listed Sessions, and failed records remain visible. Refresh reads a fresh snapshot without starting work. The same directory is available from floating conversations; returning from global links restores the preceding directory.

Click the **Links** filter beside search, or press `Ctrl+F`, to select **All**, **Created**, **Read**, **Watch**, or **Interject**. Use ↑/↓ or Tab/Shift+Tab and Enter, or click an option; **Back** or Esc cancels. Graph, list, and relation directories share the same `link:` search condition. Applying a relation retains other search terms and keeps the selected Flow if it still matches.

Inside a main or floating conversation, `:links`, **Flow Links** in `Ctrl+P`, or `Ctrl+Shift+G` opens the focused flow's neighbors. The expanded Overview rail retains up to three links from the last neighborhood read, labelled with its snapshot scope. Click a neighbor to focus it; **Links** opens the full keyboard-accessible directory. Commands and the palette remain available when the sidebar is collapsed or the terminal is narrow.

Focus places Goal, Plan, Todos, neighborhood, and Context/MCP to the left of the conversation. The transcript, composer, quotes, and pending queue stay inside the remaining width. Click the padded **Overview** / **Activity** buttons, use `Ctrl+Shift+A`, or submit `:activity` to switch between overview information and task/process controls. `F2` and `:sidebar` collapse or expand the rail. These controls affect only the current view; switching does not change execution, discard drafts, or remove pending requests. Narrow terminals use collapsed strips and retain command and palette navigation.

The **Inbox** above the composer shows pending user input and background notifications in both main and floating views. Use `Shift+Tab` or click a row to focus it, then `↑`/`↓` to select. Larger queues show page counts and **Prev** / **Next** buttons; `PgUp` / `PgDn` changes pages while the Inbox has focus. Each view retains its own page, and browsing does not consume messages or limit how many the flow can receive. User input supports `Enter` / `e` to edit, `i` to insert at the next eligible model call, `Ctrl+↑` / `Ctrl+↓` to reorder next-turn input across page boundaries, and `Delete` / `Backspace` to cancel it. Each action is also clickable. The editor supports multiline text, mouse cursor placement, and `Ctrl+S` / `Enter` or **Save** to save; `Esc` or **Cancel** discards the edit. Failed saves retain the draft. A saved insertion remains pending until the flow actually receives it; incompatible invocation settings leave it queued. Watcher, peer, timer, and internal notifications are labelled by source and cannot be edited as user input.

Use `:inbox`, **Inbox History** in `Ctrl+P`, or the pending Inbox header to browse the current flow's delivery history. **Received** and **Sent** switch with Tab/Shift+Tab or the direction button. Type to search the current page; Up/Down selects records, PgUp/PgDn or the page buttons loads newer/older pages, and Enter or a record click opens details. Details show the full body, quote, attachment count, source identities, local timestamps, and delivery receipts; Up/Down, PgUp/PgDn, and the mouse wheel scroll. **Refresh** or Ctrl+R returns to the newest recorded page. Esc returns from details, then closes the directory. Reading does not consume messages, resume execution, or mark work complete. Main and floating views use their own bound flow.

Use `:worktrees` or **Worktrees** in `Ctrl+P` to browse project resources. **More** or PgDn discovers additional Sessions and recorded workspace bindings; type to search loaded rows. Enter or click a project to inspect its registered checkouts, then select a checkout to see its observed branch, HEAD, changes, recorded lifecycle, retention, and live or released Flow references. Reference rows open the conversation with Enter or a click, or a floating view with Ctrl+O or the **Window** button. Recorded bindings remain navigable when a registry or checkout is missing. Esc returns to the previous directory with its filter and discovery progress intact. **Refresh** or Ctrl+R reloads the snapshot; refreshing the global directory restarts discovery. Main and floating views share these controls. Browsing does not allocate, release, reconcile, or change workspace ownership.

Use `:workspace` or **Flow Workspace** to inspect the focused Flow's recorded checkout binding and allocation or shared origin. **Inspect resource** opens current resource details; shared origins open their source Flow. A Flow without a managed worktree explicitly shows no binding. This historical request does not assert a live resource reference or a currently available checkout. Missing directories remain readable, and a failed resource lookup leaves the binding page available.

Select a read-only workspace property or catalog error and press Enter, click the row, or use **Read** to open its complete text. Arrow keys, PgUp/PgDn, Home/End, and the mouse wheel scroll the shared detail reader. **Back** or Esc returns to the selected record without changing its stored data. Inbox details use the same reader.

Resource details offer **Retain checkout** and, while Flow references remain bound, **Use automatic cleanup**. Enter or click applies the displayed choice after rechecking the recorded ownership under lock. The update takes effect after persistence succeeds; a stale snapshot requires refresh. Automatic cleanup waits for the last reference to close and preserves uncommitted changes. Invocation-owned leases and terminal lifecycle states display why retention cannot be changed from this view.

After all live references close, **Remove checkout…** opens a confirmation with the complete checkout path and removal scope. Tab/Shift+Tab selects controls, Enter or Space activates them, and mouse clicks use the same actions. **Force No** protects uncommitted files; changing it to **Force Yes** explicitly permits discarding them. **Remove** submits the observed record to the resource owner, while **Back** or Esc leaves the confirmation without cancelling Flow execution or an already submitted removal. Failures keep the snapshot and a scrollable error; **Refresh** or Ctrl+R reads current details before another confirmation. A pending release keeps its recorded force option. Named branches and Flow history remain; detached commits can become unreachable, so create a branch first if they must be kept. Main and floating views share this confirmation and acknowledgement path.

Use `:watchers` or **WatchHub** in `Ctrl+P` to inspect the current flow's subscriptions. Type to search, use Up/Down and PgUp/PgDn to select records, and press Enter or click a record to view its details. **Refresh** or Ctrl+R reads a new directory snapshot. Details show registration state, delivery counts, local timestamps, and source positions; Up/Down, PgUp/PgDn, and the mouse wheel scroll long content. Tab selects **Recipient** or **Source**; Enter opens that flow and Ctrl+O opens it in a window. The padded target buttons also open flows by mouse. Esc returns to the directory without cancelling a subscription or stopping execution. Main and floating views inspect their own bound flow.

Choose **Global** or press Ctrl+G to browse subscriptions across all persistent Sessions. **This Flow** or Ctrl+G returns to the original recipient. Global search filters loaded records; **More** or PgDn reads another bounded batch, and the footer reports whether more records remain. Unavailable Sessions and Flows appear as error rows. Details and cancellation retain the selected subscription's full owning Flow address, even when different Flows reuse a subscription ID. Cancellation updates that owner's snapshot in place without resetting the global filter. **New** still creates a subscription for the original view's recipient, shown above the form.

Active subscriptions have a padded **Cancel Del** button. Click it, press Delete, or select it with Tab and press Enter to cancel the registration. The details wait for host acknowledgement before showing the updated status; failures remain visible and can be retried. Cancellation stops future matches while retaining notifications already accepted by the recipient. It does not stop the source process, close a flow, or resume paused execution.

Choose **New** or press Ctrl+B in WatchHub to create a Flow subscription. Select its source from the Flow list; Ctrl+S opens Sessions, and search and pagination retain their normal controls. The form supports final replies with an optional text match, turn completion with an optional turn UUID, and instance closure. Choose once or persistent delivery and optionally enter an expiry in seconds; blank leaves the subscription without a deadline. Tab/Shift+Tab moves between fields, Left/Right changes selectors, and text fields support mouse positioning and paste. **Create** or Ctrl+S submits to the original recipient shown above the form. Failures keep the draft and registration identity for retry; editing registration content creates a new request. Cross-session subscriptions require an existing Watch grant. Esc returns without cancelling an already submitted registration.

In the source picker, **Processes** or Ctrl+B switches to the recipient's running Bash and Terminal resources; the same control returns to Flows. Search, mouse selection, and PgUp/PgDn use the shared directory. Select a process and enter text to match in its future output. The form shows its kind and incarnation, supports once or persistent delivery, and has no default expiry. Registration checks the selected process lifetime; a replaced or unavailable resource requires refreshing the source list. Creating a watcher does not resume a paused recipient. Main and floating views use their own recipient's resources.

For an active subscription, choose **Edit** or press E in its details. Change the source, condition, delivery mode, or expiry, then use **Save** or Ctrl+S. Unchanged expiry keeps the original deadline; changing it starts the entered duration when submitted. Saving replaces the registration atomically and retains notifications already received. No-change submissions do nothing, and errors keep the draft for retry. Successful saves open the new registration's details in the original directory, preserving global search and loaded records. Esc leaves the form without cancelling a save already submitted. The final-answer filter also supports excluding a specific turn UUID.

In Links, **Neighbor** opens the other endpoint of the selected flow's relation. Use Tab/Shift+Tab or click the endpoint button to choose **Source** or **Target**, then Enter or Ctrl+O to open that endpoint in the main workspace or a window. **All**/Ctrl+G shows all links recorded in the current Session. Esc returns to the previous Flow page with its search and selection intact.

Choose **Global** or press Ctrl+B in Links to include records owned by other Sessions. The directory loads in batches; use **More**/PgDn to continue, even when a batch contains no matching relations. Failed Session reads appear as error rows and do not hide the successful results. **Session**/Ctrl+B returns to the original Session scope, and Refresh restarts the global scan.

Choose **Atman Session** from `Ctrl+P` or use `:atman` to open the profile's management conversation. First use creates a projectless Session and prepares its default agent using the current view's configuration. Later visits use the saved configuration and selected default flow. Navigation preserves drafts, does not start model work, and does not resume paused or closed execution. Cross-session access still requires explicit management grants.

In FlowHub, choose **Browse sessions**, then **Management** to inspect the profile binding and explicit grants. Each grant identifies its source Flow, target scope, and permissions. Enter or click a grant to inspect it; read-only fields open a scrollable full-text view. **Revoke grant** removes that grant against the displayed profile revision and returns to the directory after persistence succeeds. Failed requests remain available for inspection and retry; refresh before retrying a changed revision. Other grants and implicit same-session access remain, and revocation does not cancel execution. Main and floating conversations share these controls. Inspecting an unconfigured profile does not create a management Session; **Open management** initializes it explicitly. The Workspace rail's **Atman** entry opens that management conversation directly.

**New grant** and **Edit grant** open a shared permission form. Choose a source Flow from the bound management Session, then choose a target Flow, a target Session, or all Sessions on this host. Source and target pickers use the searchable FlowHub directories without opening a conversation or starting execution. New grants include no permissions until selected. Tab or ↑↓ selects fields; Enter, ←→, or a click changes their values. **Save** and Ctrl+S submit the same operation and close the form only after persistence succeeds. Failed saves retain the draft and request identity; after a profile revision conflict, cancel and reopen from a refreshed directory. Esc returns from a picker to the draft, or from the form to its original directory.

**Change session** binds management to another existing persistent Session; **Select existing session** is also available before management is initialized. Choose a Session from the searchable directory, review the old and selected UUIDs and the number of removed grants, then click **Save** or press Ctrl+S. Saving atomically replaces the binding and removes its explicit profile grants; permissions are not transferred. Session-level grants and execution remain unchanged, and the current conversation stays open. Selecting the current binding is rejected without changing its grants. Failed saves retain the selection; after a revision conflict, return to Management and reopen the form. A missing bound Session can be replaced explicitly while the profile journal remains readable.

FlowHub lists and graphs show the latest timestamped activity in local time with an explicit UTC offset. The timestamp comes from committed Flow facts, including input acceptance, observed execution results, and watcher notifications. Reading a directory or recovering an interrupted turn does not create new activity. Untimed records display **Last activity: no timestamp**. Activity time does not indicate whether a Flow is currently running.

The new view opens a welcome screen with up to five recent persistent sessions and original histories from the same project. Use Shift+Tab, arrow keys or 1–9, then Enter to open a card; a mouse click selects it and a double-click opens it. Selection reuses an existing main view when available, retaining its draft. **All Projects** opens Project Hub; `:sessions` opens the conversation picker. Recent activity is a filesystem timestamp hint from the stored logs; browsing these cards does not load other execution owners or write history caches. Original and persistent sessions with matching UUIDs retain separate destinations.

Session listings default to the current project. Use `atman session list --all` for every project or `--project <path>` for an explicit project root. Session metadata retains a title and project root; manual rename is persistent, while automatic naming cannot overwrite a user title. Daemon clients can pass `project_root`, `search`, and `limit` to `list_sessions`, which includes legacy history and persistent catalogs. Persistent rows expose their default flow and flow count; an idle instance remains available, while `detached` means execution state has not been observed by this daemon.

`session list` includes persistent session catalogs and flat legacy history; `--all` also includes legacy sessions without project metadata. `session show <uuid>` displays a persistent catalog's title, project, default flow, and registered flow IDs without opening execution; if both layouts contain the UUID, both are displayed. Recorded projects remain accessible after their directories disappear. Incomplete journal tails are reported and left unchanged; catalog inspection does not resume work or modify configuration.

`memory.spec.*` stores runtime state in JSONL according to `[storage] scope`: the central data directory's `projects/<fingerprint>/specs` by default, or `<project>/.atman/specs` with `scope = "local"`. `memory.spec.materialize(feature, phase)` writes `research.md`, `design.md`, `testing.md`, or `retrospective.md`; omitting `phase` or using `implementation` writes the aggregate `IMPLEMENTATION.md`. It returns a revision, and a stale `expected_revision` rejects the write instead of overwriting edits. Run `/spec <request>` when a structured requirements interview and explicit design confirmation are useful; the managed agent does not start this workflow automatically. A main-agent design review uses `form.ask` with a `spec_review` confirmation binding and passes the returned single-use `confirmation_id` to `memory.spec.review`; cancellation records no decision, and a stale or mismatched revision requires another confirmation.

## 9. Where to go from here

- **`atman monitor`** starts an HTTP UI at `http://localhost:65098/` showing every session's event stream with FTS5 search.
- **`atman preview serve`** starts the built-in artifact workbench at `http://127.0.0.1:65097/`; `preview.push` starts it automatically on the default local URL and saves topics in the selected project scope.
- **`atman logs stream <session>`** tails a running daemon's SSE feed in the terminal. Add `--instance <flow-uuid>` to read committed persistent-flow batches, and `--after '<journal-uuid>:<sequence>:<digest>'` to resume after a saved batch receipt. Instance reads require an explicit session UUID and cannot use the legacy `--since-seq` option.
- **`atman sync init <url>`** turns `<project>/.atman/` into a git repo so your memory travels across machines.
- **`atman migrate list --from opencode`** lists external sessions. Use `atman migrate import <id> --from opencode|kiro-cli --into new` to retain a text transcript as a read-only persistent archive, then open its printed ID with `atman --continue <id>`. `--out <path>` exports normalized JSONL instead.
- **`atman migrate inspect <session>`** prints a read-only JSON map of legacy Atman history, including record byte ranges, ownership, and malformed or unassigned records. It preserves source files and does not start flows.
- **`atman migrate archive <session> --confirm-stopped`** imports a stopped legacy session into independent read-only flow journals. The JSON response includes the new session ID and source mapping. Retries resume the same destination; `--into <uuid>` explicitly selects another destination. Original files remain unchanged, and missing project directories do not prevent history import. Open the returned ID with `atman --continue <imported-session>` to browse it in the TUI without model configuration. Non-TUI mode prints the default flow's archived conversation and exits.

From a persistent TUI, use **Switch Session** in `Ctrl+P` or `:sessions` to find the imported session and open its default Flow directly. FlowHub provides selection of its other Flows and floating windows. Archived views show their read-only state and do not start models or tools. Switch back to an executable flow to send messages; drafts stay with their original views.

The Sessions directory also includes original legacy histories, including histories without project metadata. Each row retains its storage source, so an original and a persistent session with the same UUID remain separate entries. Original histories open through their flow ownership directory. Listing sessions reads metadata without loading conversation bodies; missing or damaged entries remain visible for inspection and report errors when opened. Search applies to the displayed page; use the page controls to browse additional sessions.
- **[docs/context-strategy.md](./context-strategy.md)** covers the goal / todos / sliding-window / recall / compaction layering and when to reach for each.
- **[docs/how-to-filter.md](./how-to-filter.md)** covers list combinators, fanout, and sequential expressions.
- **`examples/`** in the atman source tree has larger canonical flows (agent loop, hunk review, LSP-style code review, etc).

Atman processes using the same preview `base_url` share one server. Projects appear after their first `preview.push`; use the left rail, or open the menu and choose **All Projects** in a narrow window, to switch between them.

HTML artifacts run their scripts in an isolated iframe without access to Atman's page or local APIs. HTML topics use the available canvas width; **Full width** (or `F`) folds project navigation, topic list, and inspector into drawers. Use the menu and inspector buttons to reopen them, **Show panels** to restore the regular layout, or `Esc` to leave full-width mode.

If another preview service already owns port 65097, `preview.push` continues to use that service when its API is compatible. To use the built-in workbench on another port, run `atman preview serve --port 65107` and set `[preview] base_url = "http://127.0.0.1:65107"` in the Atman config; custom URLs require an explicitly running server.

## Troubleshooting

- **"no route matched"** — REPL doesn't know what to do with your bare text. Check `~/.config/atman/routes.at`, or use `/name args...` for a command flow.
- **`unreachable: connect: ...` on a provider row** — check the base URL and that you can `curl` it. Corporate proxies + custom CAs need `SSL_CERT_FILE`.
- **REPL prints nothing after your input** — you're in the agent loop. Watch `atman logs tail --follow` or `atman monitor` to see what's happening.
- **Agent forgets what you asked two turns ago** — set a `:goal`. `commands/agent.at` is a managed atman template and is overwritten on agent start; to customize behavior, create your own `.at` file and point `routes.at`'s `default_route` at it. See `docs/context-strategy.md` for context-layer options.
