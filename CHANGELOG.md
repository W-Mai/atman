# Changelog

All notable changes to atman are documented in this file.

---

## [1.1.1] — 2026-07-22

- **crates.io metadata** — keywords, categories, and richer descriptions for all six crates. Homepage and repository links added to every crate manifest.

## [1.1.0] — 2026-07-22

### Runtime

- **LLM stall timeout** — detect and auto-retry when an LLM streaming response hangs mid-generation. Configurable timeout with 5 test scenarios covering edge cases.
- **Interjection overhaul** — L1 nudge and L3 redirect now work correctly during active LLM streaming. HardStop (`!stop`) reliably cancels flows and clears stale pending approvals. Injection queue row calculation fixed; re-broadcast on every state change keeps the TUI in sync. `select!` bias removed so injections don't starve during streaming. L1 nudge freeze (events channel `Closed` spin) fixed. Interjection paths no longer contend on `compact_lock`.
- **Cancelled flow propagation** — `FlowStatus::Cancelled` added to the event model. Cancellation now cascades from parent to all child subflows. `Value::Err(Cancelled)` unwrapped correctly in the executor top-level path.
- **Session pwd tracking** — the actual working directory at session creation time is recorded in `SessionMeta.start_path`. `atman session new` and `atman session move <sid> <cwd>` commands added. Working directory is now injected into the AI context so models know where they are operating.
- **System context injection** — system prompt context moved from the messages array into `req.system`, matching provider-native placement.

### TUI · Workflow Panel

- **Workflow panel state machine rewritten** — 12 comprehensive unit tests cover the full lifecycle. Double-panel creation, phantom panels from non-FlowStart events, and FlowDone prematurely closing parent panels are all fixed.
- **Subflow routing** — subflow `FlowStart` events route to their parent panel instead of creating orphaned panels. Recursive subflows reuse the parent panel. `FlowDone` is routed to the correct panel via a `HashMap<run_id, panel_idx>` lookup.
- **Course-correct restart** — cancelled workflow panels now reopen on course-correct restart instead of creating duplicates. The cancelled flag is propagated through history replay and the streaming `FlowDone` path.
- **Cancelled workflow distinction** — cancelled flows show a yellow "Cancelled" label in the workflow panel instead of the normal spinner.
- **Pending interjection queue** — L1/L2/L3/L4 interjections are displayed above the input box with color-coded level badges, subscribing to the `injection_rx` broadcast channel.

### TUI · Input Editor

- **Word-wrap aware cursor** — cursor positioning and mouse clicks now use visual (wrapped) coordinates matching ratatui's `Wrap { trim: false }` rendering. Logical→visual line mapping, char-level word-boundary wrapping, and reverse lookups all handled in `compute_wrapped_lines`.
- **❯ prompt moved to block title** — the `❯` prompt lives on the left border of the input block, aligned with the first text line. Renders DIM during streaming, BOLD when idle.
- **Cursor stays visible during streaming** — the `!app.streaming` guard that hid the cursor while the model was generating has been removed.
- **Cursor x-offset fix** — horizontal cursor position now correctly accounts for border + padding (+2 px, not +1).

### TUI · Sidebar

- **Collapsible sidebar sections** — click `▸`/`▾` triangles toggle each section (Goal, Plan, Todo, Meta). Sidebar auto-collapses on narrow terminals; a lock icon pins it expanded.
- **Sidebar meta section** — shows project directory, atman version, and update availability at the bottom of the sidebar.
- **Layout fixes** — Meta section no longer gets pushed out of view; Goal / Plan / Todo sections have proper spacing; divider→context gap corrected; semver comparison fixed.

### TUI · Border & Pulse

- **Pulse bar** — a breathing bottom border animation when workflows are running. The pulse bleeds into the vertical borders, fading upward from the bottom edge. Colors lerp from the active border color rather than a hardcoded value.
- **Smooth border color transition** — when streaming state changes, the border fades between trust-mode color and subtle accent over 350 ms instead of snapping instantly.
- **Trust mode border colors tuned** — desaturated and brighter across all modes; the orange (local mode) renders as actual orange instead of yellow.

### TUI · State Persistence

- **Runtime UI state persisted across sessions** — trust mode, theme, sidebar visibility, and section collapse states are saved to disk and restored on the next launch. State is written on every relevant toggle without blocking the UI thread.
- **`ModeColorExt` trait** — `ModeColor::ratatui()` converts the internal color representation to a ratatui `Color` in one call, replacing ad-hoc conversions.

### TUI · Session Management

- **Ctrl+P command palette** — entries grouped by category. NewSession, MoveSession, and Delete Session actions added alongside existing shortcuts.
- **Session switcher** — keyboard shortcuts moved from the title bar to the footer row for a cleaner look.

### TUI · Other Fixes

- **Ctrl+C deadlock fix** — key priority corrected: input handling > stop signal > quit. Ctrl+C no longer deadlocks during streaming.
- **Redundant comments removed** from workflow event routing code.

### CLI

- **Streaming suggest** — `:suggest` now streams the meta-LLM response through a TUI form modal for accept / edit / reject, replacing the old blocking flow.
- **`atman init` output simplified** — dynamic config path display, fewer redundant steps.

### Release & Distribution

- **cargo-dist setup** — 7-platform release pipeline with shell installer, PowerShell installer, MSI, and Homebrew tap.
- **Windows target dropped** — `atman-daemon` uses Unix-only APIs.
- **Homepage** — switched to `atman.run` custom domain with SEO-optimized meta tags and the atman logo as favicon.
- **CI fix** — oranda installed via `cargo` instead of the defunct `install.axo.dev` host.
- **README** — tier table, crate count, `edit_and_verify` snippet, and quickstart version string corrected.

### Tests

- `slash_command_resolver_accepts_multi_flow_agent_at` no longer hangs when no API key is configured — replaced `wait_with_output()` with a 3-second `try_wait()` loop.

---

## [1.0.0] — 2026-07-05

Initial public release. See `docs/quickstart.md` for setup and `examples/` for canonical flows.

- **atman DSL** (`.at`): parser, AST, pretty-printer — `flow`, `llm`, `contract`, `subflow`, `fanout`, `watch`, `when`, `retry`, `fallback`.
- **Runtime**: eager-binding evaluator, tool dispatch, provider dispatch (Anthropic + OpenAI-compatible), executor with event sink and `events.jsonl` append-only log, memory stores (todo / plan / goal / confession), user interjection (L1 nudge + L4 hard stop), simple context truncation.
- **Tools**: `fs.read` / `fs.write` / `fs.edit` / `fs.list` / `fs.grep`, `bash.spawn` / `bash.status` / `bash.output` / `bash.kill`, `term.spawn` / `term.input` / `term.capture`, `web.fetch` / `web.search`, `git.log` / `git.diff` / `git.status` / `git.add` / `git.commit` / `git.branch` / `git.push`, `memory.*`, `plan.*`, `agent.spawn`, `form.ask`, `preview.push`, `session.push`.
- **CLI**: `atman` binary with subcommands (`run`, `logs`, `session`, `cost`, `doctor`, `init`) plus interactive REPL with slash commands and `:goal`, `:mode`, `:exit` meta-commands.
- **TUI**: ratatui-based terminal UI with streaming output, markdown rendering, approval modals, history search, and boot animation.
- **Preview server**: `atman preview` serves rendered artifacts (markdown, mermaid, HTML, diff) on port 65097 for browser review.
