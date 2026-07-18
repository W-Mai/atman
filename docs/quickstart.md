# atman quickstart

From zero to a running code agent. Reads top-to-bottom in ~10 minutes.

## 1. Install

Prereqs: rustc 1.85+, git, and (optional but recommended) an Anthropic or OpenAI API key.

```bash
git clone <your atman checkout url> ~/src/atman
cd ~/src/atman
cargo install --path crates/atman-cli
```

`cargo install` drops the `atman` binary into `~/.cargo/bin`. Make sure that's on your `$PATH`.

Verify:

```bash
atman version
```

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
atman v1.0.0 — type `:help` for commands, `:exit` to leave
[atman] session=… events=/…/events.jsonl
atman ready. `/hello` for a smoke test, plain text to chat.
atman>
```

Three input modes:

- `:name`     — REPL builtin (`:help`, `:exit`, `:cost`, `:goal`, `:suggest`, …).
- `/name arg` — run `~/.config/atman/commands/<name>.at`.
- plain text — routes.at kicks in. Anything unmatched falls into the code agent flow.

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

The default agent reads the sliding window of your last 10 messages and forgets anything older. When the agent needs an anchor that can't get evicted — "what am I actually trying to accomplish this session" — set a goal:

```
atman> :goal ship the atman agent MVP by friday
[atman] goal set: ship the atman agent MVP by friday
atman> :goal
[atman] goal: ship the atman agent MVP by friday
atman> :goal clear
[atman] goal cleared
```

`:goal` is stored in `<session_dir>/goal.txt` and auto-injected as a system-prompt prefix on every LLM call in this session. It never enters the message list, so context compaction, sliding window, and recall never touch it. See `docs/context-strategy.md` for the O(N²) cost math that motivated this design.

The agent also has `memory.todo.set` / `memory.todo.done` in its tool list, so it will break multi-step tasks into todos on its own — goal is the north star, todos are the plan.

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

## 8. Where to go from here

- **`atman monitor`** starts an HTTP UI at `http://localhost:65098/` showing every session's event stream with FTS5 search.
- **`atman logs stream <session>`** tails a running daemon's SSE feed in the terminal.
- **`atman sync init <url>`** turns `<project>/.atman/` into a git repo so your memory travels across machines.
- **`atman migrate list --from opencode`** imports opencode / kiro session transcripts into a fresh atman session.
- **[docs/context-strategy.md](./context-strategy.md)** covers the goal / todos / sliding-window / recall / compaction layering and when to reach for each.
- **[docs/how-to-filter.md](./how-to-filter.md)** covers the list combinators plus the pipe operator.
- **`examples/`** in the atman source tree has larger canonical flows (agent loop, hunk review, LSP-style code review, etc).

## Troubleshooting

- **"no route matched. add `\"prefix\" -> command` to ~/.config/atman/routes.toml"** — REPL doesn't know what to do with your bare text. Either add a route or `atman init` again to write the `default_route { flow: agent }` fallback.
- **`unreachable: connect: ...` on a provider row** — check the base URL and that you can `curl` it. Corporate proxies + custom CAs need `SSL_CERT_FILE`.
- **REPL prints nothing after your input** — you're in the agent loop. Watch `atman logs tail --follow` or `atman monitor` to see what's happening.
- **Agent forgets what you asked two turns ago** — set a `:goal`, or if that's not enough, edit `commands/agent.at` and raise the `memory.recent_turns(n: 10)` window. See `docs/context-strategy.md` for when to escalate to layer 2 (recall).
