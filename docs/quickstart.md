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

`llm.call(retry: N)` retries transient network failures, timeouts, rate limits, and provider outages. Authentication, invalid requests, and unsupported tool schemas fail immediately; `retry_classified: [...]` explicitly selects other error categories when a flow requires them. Context-overflow recovery still compacts the input before resending. Requests never switch API protocol or reasoning effort in response to a protocol rejection.

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

Tool-call purposes become the primary labels in output blocks, workflow nodes, approval rows, sub-agent panels, task windows, and the Tasks activity area. The tool name and source handle remain visible as secondary technical metadata. Expanded Bash and Terminal output blocks and their floating task panels also show the original spawn command for auditing. The managed agent writes these dynamic labels in the current user's language when practical. The Tasks activity area shows running leaves, so a dispatcher is hidden while its concrete parallel tool calls are active.

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

Press `Ctrl+L`, choose **All Projects** from `Ctrl+P`, or run `:projects` to open Project Hub. Use the arrows to select a project, `Enter` to inspect its sessions, `P` to pin or unpin it, and `X` twice to archive it. An archived project can be restored with `X` or deleted with `Delete`/`Backspace` twice. Delete removes Atman-owned sessions, goals, plans, tasks, confessions, specs, previews, and indexes while preserving the source directory, Git repository, and hand-written `.atman` configuration, commands, skills, flows, and rules. The same actions are clickable. A missing project path still allows stored sessions to open; workspace tools report the unavailable path when used.

Choose **Set Project Storage Scope** from `Ctrl+P` or run `:storage` to select where new sessions for the current project store Atman data. `global` uses the central Atman data directory; `local` uses `<project>/.atman`. `:storage global` and `:storage local` apply the choice directly. The setting is saved in the project's `.atman/config.toml` and takes effect for new sessions.

## 6. Anchor the agent on a session goal

The default agent uses the active session message window. As the conversation grows, automatic compaction replaces older ranges with an operational summary while retaining recent turns. Put the objective that must remain explicit outside that lossy window in the session goal:

```
atman> :goal ship the atman agent MVP by friday
[atman] goal set: ship the atman agent MVP by friday
atman> :goal
[atman] goal: ship the atman agent MVP by friday
atman> :goal clear
[atman] goal cleared
```

`:goal` is stored in `<session_dir>/goal.txt` and appended to the system context of every LLM call running with this session. It does not enter the message list, so message compaction does not rewrite it. See `docs/context-strategy.md` for the complete request structure and compaction model.

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

The managed agent exposes `mcp.status`, `mcp.tools`, `mcp.await`, and `mcp.call`. The request's MCP definitions stay fixed as the catalog grows. Tool descriptions and schemas are retrieved as tool results, so discovery does not replace the model's existing tool prefix.

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
