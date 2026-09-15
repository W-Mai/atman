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

Compatible OpenAI gateways that implement `reasoning_effort` can select that
wire format independently of the endpoint type:

```toml
[providers.gateway]
kind = "openai-compat"
base_url = "https://gateway.example/v1"
reasoning_format = "reasoning-effort"
# Enable only when the gateway accepts OpenAI's prompt_cache_key field.
# prompt_cache_key = true
```

The default OpenAI API endpoint and Codex enable stable prompt-cache routing by
default. Custom OpenAI base URLs and OpenAI-compatible gateways leave it disabled unless
`prompt_cache_key = true` is configured explicitly.

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

Reasoning can also be selected for one run with `--reasoning high`; repeat
`--image path/to/image.png` to attach images. In the TUI, Cmd+V, Ctrl+V, or Alt+V
attaches the clipboard image. Its `[image N]` reference and attachment bar remain
visible until submit; deleting the reference removes that image. Alt+Delete
removes the latest pending image. The input border shows the effective reasoning
depth for the next submission, and Ctrl+T cycles that input value. It is exposed
to the flow as invocation-local `env("effort")` data. `llm.call`, `llm.extract`,
`llm.classify`, and `llm.generate_branches` consume it only when their own call
explicitly passes `effort: env("effort")`; inheriting the invocation environment
alone does not change a request.

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

- Type a normal message to place it in the ordered `next` queue without interrupting the current flow. The next task-facing `llm.call` consumes eligible queued messages as trailing user context; if the flow makes no further call, they run as new turns afterward. Commands, path attachments, and submissions with an explicit reasoning setting retain their new-turn behavior. Press `Shift+Tab` to focus the queue, use `↑`/`↓` to select, `Alt+↑`/`Alt+↓` to reorder, `Enter` or `e` to edit, and `Delete` or `Backspace` to remove. The same actions are available with the mouse.
- `!nudge <text>` — L1 nudge (added to context at the next LLM node boundary).
- `!course-correct <text>` — L2 (mid-stream restart with the correction).
- `!redirect <flow>` — L3 (switch to another flow).
- `!stop` — L4 (kill immediately).

A `form.ask` that times out can still be answered while its form remains open. The answer is recorded with the original question and added to the next task-facing `llm.call` as user context; it does not start or interrupt a flow by itself.

Press `Ctrl+L` to open Project Hub. Use the arrows to select a project, `Enter` to inspect its sessions, `P` to pin or unpin it, and `X` twice to archive it. An archived project can be restored with `X` or deleted with `Delete`/`Backspace` twice. Delete removes Atman-owned sessions, goals, plans, tasks, confessions, specs, previews, and indexes while preserving the source directory, Git repository, and hand-written `.atman` configuration, commands, skills, flows, and rules. The same actions are clickable. A missing project path still allows stored sessions to open; workspace tools report the unavailable path when used.

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

**Regression-test a flow** (offline; uses a mock provider):

```bash
atman flow test ~/.config/atman/commands/hello.at
```

First run writes `hello.at.snap.json`. Subsequent runs compare the current output to the snapshot; mismatches print one line per drift case and exit non-zero. Re-run with `--bless` when the change is intended.

### MCP readiness and direct calls

An `llm.call` that includes `"mcp.jira.search"`, `"mcp.jira.*"`, or `"mcp.*"` waits for the referenced server set before building the provider request. At code can inspect or invoke a server without an LLM:

```at
ready = mcp.await(server: "jira")
tools = mcp.tools(server: "jira")
result = mcp.call(server: "jira", tool: "search", input: {query: "open"})
```

The CLI supports the same direct operation with JSON input. Tools declaring the MCP `readOnlyHint` annotation can run directly; tools without that declaration require the explicit write override:

```bash
atman mcp call jira search '{"query":"open"}'
atman mcp call jira update_issue '{"key":"PROJ-1","summary":"Updated"}' --allow-write
```

## 8. Session discovery and spec state

Session listings default to the current project. Use `atman session list --all` for every project or `--project <path>` for an explicit project root. Session metadata retains a title and project root; manual rename is persistent, while automatic naming cannot overwrite a user title. Daemon clients can pass `project_root`, `search`, and `limit` to `list_sessions`.

`memory.spec.*` stores runtime state in JSONL according to `[storage] scope`: the central data directory's `projects/<fingerprint>/specs` by default, or `<project>/.atman/specs` with `scope = "local"`. `memory.spec.materialize(feature, phase)` writes `research.md`, `design.md`, `testing.md`, or `retrospective.md`; omitting `phase` or using `implementation` writes the aggregate `IMPLEMENTATION.md`. It returns a revision, and a stale `expected_revision` rejects the write instead of overwriting edits. Run `/spec <request>` when a structured requirements interview and explicit design confirmation are useful; the managed agent does not start this workflow automatically.

## 9. Where to go from here

- **`atman monitor`** starts an HTTP UI at `http://localhost:65098/` showing every session's event stream with FTS5 search.
- **`atman preview serve`** starts the built-in artifact workbench at `http://127.0.0.1:65097/`; `preview.push` starts it automatically on the default local URL and saves topics in the selected project scope.
- **`atman logs stream <session>`** tails a running daemon's SSE feed in the terminal.
- **`atman sync init <url>`** turns `<project>/.atman/` into a git repo so your memory travels across machines.
- **`atman migrate list --from opencode`** imports opencode / kiro session transcripts into a fresh atman session.
- **[docs/context-strategy.md](./context-strategy.md)** covers the goal / todos / sliding-window / recall / compaction layering and when to reach for each.
- **[docs/how-to-filter.md](./how-to-filter.md)** covers the list combinators plus the pipe operator.
- **`examples/`** in the atman source tree has larger canonical flows (agent loop, hunk review, LSP-style code review, etc).

Atman processes using the same preview `base_url` share one server. Projects appear after their first `preview.push`; use the left rail, or open the menu and choose **All Projects** in a narrow window, to switch between them.

HTML artifacts run their scripts in an isolated iframe without access to Atman's page or local APIs. HTML topics use the available canvas width; **Full width** (or `F`) folds project navigation, topic list, and inspector into drawers. Use the menu and inspector buttons to reopen them, **Show panels** to restore the regular layout, or `Esc` to leave full-width mode.

If another preview service already owns port 65097, `preview.push` continues to use that service when its API is compatible. To use the built-in workbench on another port, run `atman preview serve --port 65107` and set `[preview] base_url = "http://127.0.0.1:65107"` in the Atman config; custom URLs require an explicitly running server.

## Troubleshooting

- **"no route matched"** — REPL doesn't know what to do with your bare text. Check `~/.config/atman/routes.at`, or use `/name args...` for a command flow.
- **`unreachable: connect: ...` on a provider row** — check the base URL and that you can `curl` it. Corporate proxies + custom CAs need `SSL_CERT_FILE`.
- **REPL prints nothing after your input** — you're in the agent loop. Watch `atman logs tail --follow` or `atman monitor` to see what's happening.
- **Agent forgets what you asked two turns ago** — set a `:goal`. `commands/agent.at` is a managed atman template and is overwritten on agent start; to customize behavior, create your own `.at` file and point `routes.at`'s `default_route` at it. See `docs/context-strategy.md` for context-layer options.
