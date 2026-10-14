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

`llm.call(retry: N)` retries transient network failures (including SSE response-body transport failures), timeouts, rate limits, provider outages, and empty assistant replies. Empty replies do not enter session history. Authentication, invalid requests, and unsupported tool schemas fail immediately; `retry_classified: [...]` explicitly selects other error categories when a flow requires them. Context-overflow recovery still compacts the input before resending. Failed calls release the context lock before automatic compaction. Requests never switch API protocol or reasoning effort in response to a protocol rejection.

Drag across transcript text, line-end space, or short Markdown gaps to open Copy, Rich Copy, Quote, and Cancel actions; holding the pointer past the visible document edge scrolls the selection. Quote appears above the input until submission, separate from the editable prompt. Click its card or press F4 to expand it, then use PgUp/PgDn or the mouse wheel to scroll; click `[x]` or press Ctrl+U to remove it. A short terminal shows the pending quote count in the input border.

Models with image input enabled can also call `image.read(path: "...")` to inspect a local PNG, JPEG, GIF, or WebP file during an agent run. The tool imports at most 20 MiB through the session attachment store and supplies the image to the next model call without placing base64 in the textual tool result.

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
| Choose a session to delete; generate a name | `:delete-session`, `:name-session` |
| Toggle sidebar; select transcript text | `:sidebar`, `:yank` |
| Copy last assistant message or tool result | `:copy last-message`, `:copy last-tool` |
| Compact transcript | `:compact` |

Panel commands use the same actions as the palette and remain available during a running flow. Opening a panel does not submit a prompt or consume pending quotes or attachments. Floating windows support title-area dragging and bottom-right resizing; reopening preserves their position and dimensions. Narrow MCP and Knowledge windows initially use the available canvas and retain subsequent manual adjustments. A close action blocked by an unfinished edit displays a reason. Transcript commands such as `:copy` and `:compact`, and commands with arguments such as `:storage local` and `:sidebar off`, retain their existing behavior. `:help` prints command help; `:keybindings` opens the shortcuts panel. The plain REPL completes commands from the same registry and reports when a command requires the TUI.

Tool-call purposes become the primary labels in output blocks, workflow nodes, approval rows, sub-agent panels, task windows, and the Tasks activity area. The tool name and source handle remain visible as secondary technical metadata. Expanded Bash and Terminal output blocks and their floating task panels also show the original spawn command for auditing. The managed agent writes these dynamic labels in the current user's language when practical. The Tasks activity area shows running leaves, so a dispatcher is hidden while its concrete parallel tool calls are active.

Click a Flow task's sidebar cross once to stop it. Closing its task window or pressing `Esc` in that window uses the same cancellation action. Bash and Terminal crosses retain two-click confirmation. A stopping task stays visible until execution exits, then moves out of the active list while its details remain available in History.

Child flows can remain available after delivering a final answer. Their task cards show execution activity separately from handle lifetime: paused or idle children use a static waiting indicator, and running children animate. Waiting does not mark an unfinished child complete or prevent later resumption.

`Ctrl+Tab` / `Ctrl+Shift+Tab` cycles floating-window focus in creation order on terminals that report those modified keys. Theme and trust-mode pickers accept arrow keys, wheel navigation, and clicks anywhere in a padded option; `Enter` applies the selection, and **Cancel** or `Esc` closes the picker.

Try the smoke test first:

```
atman> /hello
"hello from atman"
```

Then the code agent:

```
atman> list the .at files under examples/ and pick one to summarise
[agent loops, calls fs.list, reads files, replies …]
```

While a flow is running you can:

- Type a normal message to place it in the ordered `next` queue without interrupting the current flow. The next task-facing `llm.call` consumes eligible queued messages as trailing user context; if the flow makes no further call, they run as new turns afterward. Commands, path attachments, and submissions with an explicit reasoning setting retain their new-turn behavior. Press `Shift+Tab` to focus the queue, use `↑`/`↓` to select, `Alt+↑`/`Alt+↓` to reorder, `Enter` or `e` to edit, `i` to insert the selected text task into the current flow's next LLM call, and `Delete` or `Backspace` to remove. The same actions are available with the mouse. Insertion waits in L1 without adding a transcript entry; if the turn ends before pickup, the task returns to Next.
- `!nudge <text>` — L1 nudge (added to context at the next LLM node boundary).
- `!course-correct <text>` — L2 (mid-stream restart with the correction).
- `!redirect <flow>` — L3 (switch to another flow).
- `!stop` — L4 (kill immediately).

A `form.ask` uses `↑`/`↓` to select choices and `Tab`/`Shift+Tab` to move to the next or previous question. A form that times out can still be answered while it remains open. The answer is recorded with the original question and added to the next task-facing `llm.call` as user context; it does not start or interrupt a flow by itself.

Form choices are clickable across their padded rows, including wrapped options. Clicking a multi-select option toggles it; clicking a text answer positions the cursor. Use the mouse wheel to scroll long questions. Review the final Yes/No confirmation before submitting all answers.

Compaction review provides Accept / `Enter`, Edit / `e`, and Reject / `Esc` buttons. While editing, Save / `Enter` accepts the edited summary; Back / `Esc` returns to viewing and keeps the edits for later acceptance. Click the summary to place the editing cursor, use `Shift+Enter` for a new line, and scroll either pane with the mouse wheel. In viewing mode, `↑` / `↓` scrolls the summary and `PgUp` / `PgDn` scrolls the source slice; in editing mode, `PgUp` / `PgDn` scrolls the summary.

Press `Ctrl+L`, choose **All Projects** from `Ctrl+P`, or run `:projects` to open Project Hub. Use the arrows to select a project, `Enter` to inspect its sessions, `P` to pin or unpin it, and `X` twice to archive it. An archived project can be restored with `X` or deleted with `Delete`/`Backspace` twice. Delete removes Atman-owned sessions, goals, plans, tasks, confessions, specs, previews, and indexes while preserving the source directory, Git repository, and hand-written `.atman` configuration, commands, skills, flows, and rules. The same actions are clickable. A missing project path still allows stored sessions to open; workspace tools report the unavailable path when used.

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

In the TUI, click **Pause / Resume** beside the goal or press `Ctrl+Y`. `:goal` and **Manage Goal** in `Ctrl+P` open goal details, editing, and controls for the root agent and its children. Child flow panels have their own Pause / Resume button and use `Ctrl+Y` while focused. `Tab` / `Shift+Tab` selects controls, `Enter` activates them, and `Ctrl+S` saves an edited objective. Mouse controls use the same actions.

User Pause lets the current model request finish and saves its reply, then suspends new requests and tool starts. Resume continues from that boundary. Agent completion pauses automatic continuation while allowing its final response to finish. A parent pause restricts descendants without replacing their individual states. `Esc` cancels the current turn and pauses its goal; clearing removes the goal and cancels its execution. Changing the objective text does not resume it.

Goals are stored in `<session_dir>/goal.json` with an identity, revision, and state. Existing `goal.txt` records remain readable and load paused; clearing prevents the old text from reappearing. Goal text and child outcomes enter model history through versioned context records without repeating unchanged content. See [Goal controls](./goals.md) for execution semantics and daemon methods, and [Context strategy](./context-strategy.md) for request composition.

Use the goal for the objective, `plan.write` / `plan.tick` for the high-level ordered route, and `memory.todo.*` for concrete execution items inside the current plan step. Goal and active plan are reassembled into the system context; todos remain available through tools and the UI.

## 7. First flow snapshot / test

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

Session listings default to the current project. Use `atman session list --all` for every project or `--project <path>` for an explicit project root. Session metadata retains a title and project root; manual rename is persistent, while automatic naming cannot overwrite a user title. Daemon clients can pass `project_root`, `search`, and `limit` to `list_sessions`.

`memory.spec.*` stores runtime state in JSONL according to `[storage] scope`: the central data directory's `projects/<fingerprint>/specs` by default, or `<project>/.atman/specs` with `scope = "local"`. `memory.spec.materialize(feature, phase)` writes `research.md`, `design.md`, `testing.md`, or `retrospective.md`; omitting `phase` or using `implementation` writes the aggregate `IMPLEMENTATION.md`. It returns a revision, and a stale `expected_revision` rejects the write instead of overwriting edits. Run `/spec <request>` when a structured requirements interview and explicit design confirmation are useful; the managed agent does not start this workflow automatically. A main-agent design review uses `form.ask` with a `spec_review` confirmation binding and passes the returned single-use `confirmation_id` to `memory.spec.review`; cancellation records no decision, and a stale or mismatched revision requires another confirmation.

## 9. Where to go from here

- **`atman monitor`** starts an HTTP UI at `http://localhost:65098/` showing every session's event stream with FTS5 search.
- **`atman preview serve`** starts the built-in artifact workbench at `http://127.0.0.1:65097/`; `preview.push` starts it automatically on the default local URL and saves topics in the selected project scope.
- **`atman logs stream <session>`** tails a running daemon's SSE feed in the terminal.
- **`atman sync init <url>`** turns `<project>/.atman/` into a git repo so your memory travels across machines.
- **`atman migrate list --from opencode`** imports opencode / kiro session transcripts into a fresh atman session.
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
