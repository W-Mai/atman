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
```

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
depth, and Ctrl+T cycles the session override.

Optional tool output payload limits can be set in `~/.config/atman/config.toml`:

```toml
[tool_output]
max_lines = 32
max_bytes = 1024
max_line_bytes = 384
```

`fs.read` exposes `start_byte_in_line` for continuing a long UTF-8 line; `bash.output` continues with `next_cursor`, and `term.capture` continues with its returned terminal-area coordinates.

## 4. Sanity check

```bash
atman doctor
```

You should see:

- Your `data_dir` and `config_dir` (both auto-created if missing).
- One row per provider: `[✓]` if the env var is set, `[✗]` if not, plus `reachable (HTTP …)` / `unreachable: …` for the base URL.
- Preview daemon status (optional — only matters if you use `preview.push`).
- Any migrated rules picked up from CLAUDE.md / .cursorrules / etc.

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
- `/name arg` — run `~/.config/atman/commands/<name>.at`.
- plain text — `routes.at` handles configured prefixes and its default route.

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

- `!nudge <text>` — L1 nudge (added to context on next chunk boundary).
- `!course-correct <text>` — L2 (mid-stream restart with the correction).
- `!redirect <flow>` — L3 (switch to another flow).
- `!stop` — L4 (kill immediately).

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

## 8. Session discovery and spec state

Session listings default to the current project. Use `atman session list --all` for every project or `--project <path>` for an explicit project root. Session metadata retains a title and project root; manual rename is persistent, while automatic naming cannot overwrite a user title. Daemon clients can pass `project_root`, `search`, and `limit` to `list_sessions`.

`memory.spec.*` stores runtime state in JSONL. `memory.spec.materialize` writes a reviewable `IMPLEMENTATION.md` atomically and returns a revision; passing a stale `expected_revision` rejects the write instead of overwriting edits.

## 9. Where to go from here

- **`atman monitor`** starts an HTTP UI at `http://localhost:65098/` showing every session's event stream with FTS5 search.
- **`atman logs stream <session>`** tails a running daemon's SSE feed in the terminal.
- **`atman sync init <url>`** turns `<project>/.atman/` into a git repo so your memory travels across machines.
- **`atman migrate list --from opencode`** imports opencode / kiro session transcripts into a fresh atman session.
- **[docs/context-strategy.md](./context-strategy.md)** covers the goal / todos / sliding-window / recall / compaction layering and when to reach for each.
- **[docs/how-to-filter.md](./how-to-filter.md)** covers the list combinators plus the pipe operator.
- **`examples/`** in the atman source tree has larger canonical flows (agent loop, hunk review, LSP-style code review, etc).

## Troubleshooting

- **"no route matched"** — REPL doesn't know what to do with your bare text. Check `~/.config/atman/routes.at`, or use `/name args...` for a command flow.
- **`unreachable: connect: ...` on a provider row** — check the base URL and that you can `curl` it. Corporate proxies + custom CAs need `SSL_CERT_FILE`.
- **REPL prints nothing after your input** — you're in the agent loop. Watch `atman logs tail --follow` or `atman monitor` to see what's happening.
- **Agent forgets what you asked two turns ago** — set a `:goal`. `commands/agent.at` is a managed atman template and is overwritten on agent start; to customize behavior, create your own `.at` file and point `routes.at`'s `default_route` at it. See `docs/context-strategy.md` for context-layer options.
