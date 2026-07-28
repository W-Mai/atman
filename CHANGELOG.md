# Changelog

All notable changes to atman are documented in this file.

---

## [1.3.0] — 2026-07-30

A major TUI overhaul: floating panels with drag/resize/scroll, a redesigned task panel with completed-task history, terminal emulator rendering fixes, shadow rendering with multiply blend, and full theme color migration.

---

### 🎯 Highlights

- **Floating panel system** — draggable, resizable, scrollable overlay panels for terminal, bash output, workflow activity, and task history. Panel positions and sizes persist across sessions.
- **Task history panel** — click any completed task in the History panel to reopen its detail view. Rows show kind icon, status, label, output summary, start time, and elapsed duration — all column-aligned.
- **Terminal rendering** — wide character (CJK/emoji) trailing-space bug fixed. VT100 `wide_continuation` cells are now properly skipped in all three terminal renderers.
- **Shadow rendering** — switched from linear interpolation to multiply blend so shadows never brighten the underlying surface. Added `theme.shadow` color token for consistent shadow intensity across light/dark modes.

---

### ✨ Features

#### Floating Panels

- **Drag & resize** — panels can be dragged anywhere on screen with no canvas clamp. Resize from the bottom-right `⇲` handle; minimum size enforced at 20×6. Resize dimensions display on hover.
- **Maximize** — click `▢` to toggle fullscreen; terminal panels sync PTY dimensions on maximize/unmaximize.
- **Scroll** — mouse wheel and `PgUp`/`PgDn` scroll floating panels when focused. History panel supports scroll with content-overflow clamping.
- **Size persistence** — panel sizes stored in `PersistedUiState` as a `HashMap<String, (u16, u16)>`. History panel, terminal panels, and task panels all restore their last size on reopen.

#### Task Panel

- **Borderless redesign** — `modal_bg` background, no outer border, left status bar, group headers with collapse/expand, activity foreground gradient (oldest fades to `subtle_fg`).
- **Task collapse/expand** — default collapsed 3-row card (padding + label+summary + padding). Click header to toggle; click content to open floating detail panel.
- **Kill confirmation** — two-click arm mechanism with expiry. `✕` button on each running task row.
- **Insert button** — `↵` button prefills the input with the task handle for quick `@handle` references.
- **Completed-task history** — finished tasks move to a History panel accessible from a `⊞ History` button. Sortable by end time (newest first).
- **Collapsed strip** — when the task panel is collapsed to a 5-column strip, a `⊞` icon at the bottom provides one-click history access. Hamburger position is consistent across collapse/expand states.
- **Startup hiding** — task panel hides during startup/intro animation, matching sidebar behavior. Collapsed state persists across sessions.

#### History Panel

- **Rich rows** — each row shows: bar indicator, kind icon (`$` Bash / `▶` Terminal / `⬡` Flow / `✦` Agent / `↳` Subflow / `≡` Dispatch), status icon, label + output summary, start time (`HH:MM:SS`), elapsed duration.
- **Click to open** — clicking a history row opens the corresponding task detail panel with persisted size.
- **Scroll** — wheel scroll and `PgUp`/`PgDn` navigate history with overflow clamping.

#### History Search

- **Full-text search modal** — `Ctrl+K` opens a search overlay across current and project-wide session events. Results are structured hits with markdown rendering, highlighted query terms, and `j`/`k` keyboard navigation.
- **Scrollable preview** — mouse wheel and page keys scroll the search result preview pane.
- **Click to select** — click a search result to jump to the corresponding event in the preview.
- **CJK search** — FTS5 falls back to `LIKE` for Chinese queries, since SQLite FTS5 tokenizers don't handle CJK without custom configuration.

#### Terminal Emulator Rendering

- **Wide character fix** — `TerminalCell.wide_continuation` field added (with `#[serde(default)]` for backward compatibility). All three terminal renderers (floating panel, fullscreen modal, transcript inline) now skip continuation cells, eliminating the phantom space after CJK characters.

#### Shadow & Color

- **Multiply blend** — `multiply_color(a, b)` replaces `lerp_color` for shadow rendering. `result = a * b / 255` — black stays black, shadows only darken. A `theme.shadow` color token controls intensity.
- **Theme color migration** — all `Color::White`/`Black`/`Gray`/`Cyan`/etc. named constants replaced with `theme()` equivalents across `output.rs`, `form_modal.rs`, `markdown.rs`, `floating_panels.rs`, `lib.rs`, `status.rs`, `input.rs`, and viewer modals. `lerp_color` handles named colors via a theme-aware `to_rgb` lookup table.

#### Markdown

- **Inline code wrapping** — backticks stripped from inline code; long inline code wraps at content width.

---

### 🐛 Bug Fixes

- **Shadow brightening** — `lerp_color` toward `modal_bg` could brighten black terminal backgrounds. Switched to multiply blend.
- **`Color::Reset` in shadows** — terminal-default-colored cells were assigned `code_bg` before blending, producing visible artifacts. Reset cells are now skipped in shadow/fade.
- **Panel move/resize clamp** — canvas boundary clamps removed; panels can be dragged and resized freely.
- **Maximize crash** — `sanitize_widget_edges` rect clamped to buffer area to prevent out-of-bounds writes on maximize.
- **Form modal Esc** — Esc now sends `Cancelled` answer to runtime instead of silently closing the modal.
- **Task kill poisoning** — flow cancellation could kill unrelated bash tasks. Fixed by using `child_token` in the wait loop.
- **Terminal scroll capture** — mouse wheel no longer captured by terminal panels; passes through to the document scroll.
- **Terminal refresh scroll** — terminal/bash refresh no longer forces scroll to bottom; respects user scroll position.
- **Idle CPU 100%** — orphan workflow panels on session restore caused busy loop. Fixed by closing orphan panels.

---

### ♻️ Refactors

- **Theme color unification** — ~180 named `Color::` constant usages replaced with `theme()` tokens across the entire TUI crate. Only `theme.rs` (theme definitions) and `floating_panels.rs` `to_rgb` (lerp lookup table) retain named colors.
- **`AppState::open_task_panel`** — extracted shared panel-size lookup + `open_with_size` logic into a single method, reused by task panel clicks and history row clicks.
- **`FloatingPanelHitmap`** — `render()` returns aggregated hitmap for click/hover detection across all panels.

---

## [1.2.0] — 2026-07-26

### Templates

- **Managed system prompt** — system prompt externalized to `prompts/system.md`, referenced by agent.at via `@"../prompts/system.md"`. Automatically overwritten on `atman init` and every agent start; users can freely customize it.

### Runtime

- **FileRef relative path resolution** — `source_dir` wired through runtime/cli/daemon so `@` file references resolve relative to the directory containing the `.at` file.
- **Sub-agent isolation** — sub-agent `ToolCtx` fully isolated from parent session, fixing streaming hangs and deadlocks from closed event channels.
- **Message storage overhaul** — `MessageWindow` zero-copy sliding window, separate compacted and full message caches, unified `MessageStream` read/write API. Compaction now uses separate trigger and target thresholds to prevent boundary oscillation.
- **Event model unification** — `EventEnvelope` replaces `RawEventRow`, seq/ts extracted from Event variants, `Deserialize` support added for typed event replay.
- **LogSink** — per-session `notify.log` JSONL log, independent of the TUI lifecycle.

### Provider

- **Provider panel** — async Codex model discovery, enable/disable toggles, delete provider, logout confirmation. Model discovery now only runs on explicit add or manual refresh, no longer blocking startup.
- **Model key format** — unified to `provider:full_api_slug` (e.g. `Codex:codex/gpt-5.5`). `reload_from_text` merges instead of replacing; `set_model_config` preserves previously discovered models.

### TUI

- **Input positioning fixes** — bottom-anchored layout, single-row gap between content and input, cursor animation fixes, `max_scroll_offset` off-by-one correction, stale cursor fallback restricted to empty-editor only.
- **Session switch animation** — StartupIntro fade replays on every session switch.
- **Static page cursor** — cursor now visible on the static startup page, not just during intro.
- **Preview dedup** — removed duplicate glyphs from preview scene already handled by the renderer.

### CLI

- **Generic argument passing** — key=value args passed directly as user text, synthetic `atman run /path flow=name` messages removed.

### Notify

- **Toast continuity** — `ToastCollector` buffers toasts during startup and flushes them on TUI entry, preserving boot→main continuity with zero loss.
- **Noise reduction** — internal error severity downgraded and deduplicated to cut down on meaningless popups.

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
