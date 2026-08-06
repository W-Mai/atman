# Changelog

All notable changes to atman are documented in this file.

---

## [1.6.0] — 2026-08-06

Context restoration fix, LLM streaming unification, input wrapping accuracy, and sub-agent reliability improvements. Session restore no longer loses 99% of conversation history. The two duplicated LLM streaming code paths are merged into a single `LlmStream` object. Input box wrapping matches ratatui's renderer exactly via a faithful WordWrapper port.

---

### 🐛 Fixes

- **Context restoration data loss** — root agent messages were tagged `flow_run_id: Some(...)` at write sites, causing `apply_envelope_to_messages` to filter them out on `--continue` restart. Only user messages survived (99.9% data loss). Fixed by tagging root agent messages with `None` at all three write sites.
- **Input box wrap/cursor mismatch** — the self-written `compute_wrapped_lines` diverged from ratatui's `WordWrapper` in 72% of cases. Replaced with a faithful port of ratatui's `WordWrapper::process_input` state machine. Cursor row/col, click mapping, scroll, and input box height now match rendering exactly.
- **Startup page cursor stuck** — `content_w` was derived from `transcript.width` but the startup overlay uses a narrower rect (max 72 cols). Fixed by computing `content_w` from the actual `input_rect.width`.
- **Watch persist mode stalls** — persist watcher fired once then stayed in `WatchHub` forever, causing `wait_for_watcher` to spin 30s on every call. Fixed by looping `watch_output` for persist mode.
- **Sub-agent bash output leaks** — `BashChunk`, `BashExited`, `TerminalChunk`, `TerminalExited`, and `DiffPreview` stream frames lacked `run_id`, so sub-agent tool output appeared in the main document flow. Added `run_id: Option<String>` to all five variants; TUI drops frames owned by sub-agents.
- **History timestamps not localized** — `format_started_at` and history search modal displayed UTC time without timezone conversion. Fixed with `chrono::Local`.
- **CLI stdout pollution** — `notify!` with `location = Log` was ignored by `CliSink`, causing session path and token rotation messages to contaminate machine-readable stdout. Fixed by routing `Log` to stderr.
- **Tokio worker stack overflow** — subflow recursion could overflow the default 2MB stack. Increased worker stack size.
- **Orphan SubAgentActivity on restore** — sub-agents that were still running when the session was saved appeared as active after restore. Now marked as interrupted.
- **Floating panel re-render on idle** — panels were re-rendered every frame even when content hadn't changed. Added render output cache.

### ♻️ Refactoring

- **Unified LLM streaming** — `call_and_maybe_stream_inner` (eval) and `run_streaming_once` (exec) merged into a single `LlmStream` object in `streaming.rs`. Interjection draining, stall timer, stream frame emission, and L2 restart are now in one place. `StreamOutcome` enum simplified. 12 automated tests cover all paths.
- **Sub-agent retry increased** — `retry: 3` → `retry: 12` in all four subagent flow templates (research/verify/implement/review), matching the main `agent_loop`. Sub-agents no longer die on transient stall timeouts.

---

## [1.5.0] — 2026-08-22

Sidebar redesign, MCP complete support, built-in help tool, and table auto-wrap. The right sidebar is rebuilt as two independent panels matching the left task panel's visual language. MCP gains full protocol support (resources, prompts, sampling, SSE, notifications) with a TUI manager panel and CLI commands. A docstring-driven `help.show` tool provides version-synced help content.

---

### 🎯 Highlights

- **Sidebar two-panel layout** — Goal/Plan/Todos and Context/MCP are now separate panels with `modal_bg` fill, shadow rings, and independent collapse to narrow status-dot stripes. Dynamic height allocation, scroll with fade hints, and click-to-popup for truncated items.
- **MCP complete** — tool schemas reach the LLM (critical fix), plus resources, prompts, sampling, SSE transport, notifications, env vars, and JSON config (Claude Desktop compatible). TUI panel with expandable tool list, Tab browser, add form, and hot-reload. CLI: `atman mcp add/list/remove/test/import`.
- **`help.show` tool** — docstring-driven help module with dynamic topics (tools, CLI, config, MCP) and static content (DSL syntax, features). Compiled at build time from `///` comments; staleness tests ensure coverage.
- **Table auto-wrap** — markdown tables proportionally shrink columns and word-wrap cell content, handling CJK and emoji correctly.
- **Startup freeze fix** — boot animation now consumes mouse events to prevent tty input buffer exhaustion.

---

### ✨ Features

#### Sidebar Redesign

- **Two-panel layout** — the sidebar splits into an upper panel (Goal / Plan / Todos) and a lower panel (Context / MCP), each with `modal_bg` background, `render_shadow` ring, and no borders — matching the left task panel's visual language.
- **Independent collapse** — each panel collapses to a narrow 5-column stripe (`SIDEBAR_STRIP_WIDTH`) with centered hamburger, status dots (plan steps, todo status, context budget, MCP server state), and symmetric top/bottom padding. Collapsed stripes right-align; expanded panels take full width above them.
- **Dynamic height** — when both expanded, the lower panel takes its content height and the upper takes the remainder. When any panel is collapsed, each takes only its content height. Meta (project path + version) is hidden when any panel is collapsed.
- **Hover brightening** — hamburger `≡` and panel titles brighten on hover (`heading.lerp(White, 0.3)`), matching the task panel pattern. Applied to both sidebar and task panel.
- **Scroll with fade hints** — Plan and Todo sections scroll independently. When content overflows, the first and/or last visible line is dimmed to indicate scroll direction.
- **Click-to-popup** — clicking a truncated plan step or todo item opens a small floating panel to the left of the sidebar with the full text, word-wrapped. Click outside to dismiss.
- **MCP "more" row** — a centered `···` row at the bottom of the MCP server list opens the MCP floating panel on click.
- **Meta outside panels** — project path (color-coded parent/name) and version (with `∴` update-status dot) render below the panels, right-aligned, with spacing between them.
- **Triangle unification** — expand/collapse glyphs changed from `▾`/`▸` to `⌄`/`›` to match the task panel.
- **Context restoration** — all 7 context kv-lines restored (model, window, total, cache, last, attach, memory) after being accidentally reduced to 3.

#### MCP Complete

- **Tool schema fix** — `McpToolAdapter` now stores and exposes `input_schema` and `description` from the MCP server's `tools/list` response. The LLM sees actual parameter schemas instead of empty `{"type":"object"}`.
- **Resources & prompts** — `McpClient::list_resources()`, `read_resource()`, `list_prompts()`, `get_prompt()` fully implemented. TUI Tab browser cycles Tools/Resources/Prompts with async fetching. CLI: `atman mcp resources/prompts <server>`.
- **Sampling** — MCP servers can request LLM calls via `sampling/createMessage`. A `SamplingHandler` callback wired through `bootstrap.rs` resolves the model and makes the call.
- **SSE transport** — `TransportKind::Sse` parses `text/event-stream` responses for server-initiated notifications.
- **Notifications** — `McpNotification` enum covers `tools/list_changed`, `resources/list_changed`, `prompts/list_changed`, `progress`, `log`, `cancelled`. The stdio reader loop parses and forwards all notification types.
- **Env field** — `McpServerConfig.env: Vec<(String, String)>` passed to child process via `Command::envs()`.
- **JSON config** — `mcp_servers.json` in standard `mcpServers` format (Claude Desktop / Cursor / Cline compatible). TOML `[[mcp]]` blocks remain for backwards compat. `mcp_config::load()` merges both sources.
- **MCP TUI panel** — `PanelKind::Mcp` floating panel with server table (status glyph, name, transport, tool count), expandable tool list with descriptions, keyboard navigation (`j/k`, `Enter`, `Tab`, `a`, `t`, `r`, `d`), and mouse hover/click.
- **MCP add form** — press `a` in the MCP panel to open a form modal (name, transport, command/URL, args, env, tier). Saves to `mcp_servers.json` and triggers hot-reload.
- **Hot-reload** — `TuiControl::McpReload` + `spawn_mcp_boot` re-registers MCP servers without restarting atman.
- **CLI commands** — `atman mcp add [--template <name>]`, `list`, `remove <name>`, `test <name>`, `tools <name>`, `resources <name>`, `prompts <name>`, `import <file>`. 12 curated templates (filesystem, github, gitlab, sqlite, fetch, memory, puppeteer, brave-search, google-maps, sequential-thinking, time, everart).

#### help.show Tool

- **Docstring-driven** — `#[derive(Documented, DocumentedFields)]` on `SafetyConfig`, `StorageConfig`, `TrustConfig`, `McpServerConfig`, `TransportKind`, `McpToolInfo` extracts `///` comments at compile time into `DOCS` / `FIELD_DOCS` constants. Zero runtime cost.
- **Dynamic topics** — `tools` topic renders from the live `ToolRegistry`; `cli` topic renders from `META_COMMANDS`; `config` topic renders from config struct docstrings; `mcp` topic renders from MCP types.
- **Static topics** — `dsl` (syntax reference) and `features` (capability list) embedded as markdown at compile time via `include_str!`.
- **Staleness tests** — `test_all_tools_have_description`, `test_all_config_fields_documented`, `test_tools_topic_matches_registry`, `test_cli_topic_matches_meta_commands`.

#### Table Auto-Wrap

- **`width::word_wrap`** — word-boundary wrapping for ASCII, character-boundary for CJK/emoji. Single grapheme wider than `max_w` stays on its own line (overflow allowed).
- **Table rendering** — `flush_table()` proportionally shrinks columns when `cells_total > available`, wraps each cell to its column width, and renders multi-line rows with proper alignment.
- **Consolidation** — `mermaid/sequence.rs:wrap_label` and `mcp_manager.rs` tool descriptions consolidated onto `word_wrap`.

#### Provider Manager

- **Add dialog** — `openai-compat` provider type with name, API key, base URL, models fields. Writes to `config.toml` and triggers re-registration.
- **Edit form** — Tab cycle through fields, cursor display, enabled/disabled toggle.
- **BackTab** — `Shift+Tab` (`BackTab`) key action added for reverse field navigation.

#### Startup Freeze Fix

- **Boot animation event reader** — `run_boot_animation` now spawns an event reader to consume mouse events during boot, preventing tty input buffer exhaustion that caused a CPU-0% deadlock.
- **Signal handler** — `Ctrl+C` / `Esc` / `SIGTERM` abort the boot animation early.
- **run_loop hardening** — stale events flushed on entry; hover detection skipped during startup/intro.

---

### 🐛 Bug Fixes

- **MCP reconcile nesting** — container key was nested inside itself in `build_reconcile_plan`, causing infinite recursion on certain Zod-style schemas.
- **Duplicate content on thinking retry** — thinking signature extraction from `content_block_start` prevented duplicate content blocks when retrying with thinking enabled.
- **Table overflow** — dynamic `col_min` calculation when columns are tight prevents table cells from overflowing `rule_width`.
- **Byte-boundary panic** — tool result truncation now uses grapheme-aware truncation instead of byte slicing, preventing panics on multi-byte character boundaries.
- **DSL contract scope** — contract scope and interjection marked as declarative in DSL syntax reference.

---

## [1.4.0] — 2026-08-01

Diagram rendering, code quality, and CJK correctness. Mermaid diagrams are now a first-class output type with 18 diagram types, a floating panel with split view, and scroll/pan support. ANSI code blocks, math formulas, and character-level diffs ship. A workspace-wide `width` module fixes CJK/emoji alignment across the entire TUI.

---

### 🎯 Highlights

- **Mermaid diagrams** — 18 diagram types: flowchart, sequence, stateDiagram, pie, gantt, timeline, erDiagram, classDiagram, mindmap, gitGraph, journey, quadrantChart, xychart, block, packet, sankey, requirement, architecture. Diagrams are first-class `OutputItem`s with a dedicated floating panel, Tab split view (source ↔ diagram), and horizontal/vertical scroll.
- **ANSI code blocks** — ` ```ansi ` and ` ```terminal ` fenced blocks parse VT100 escape codes and render with full color.
- **Math formulas** — `$...$` inline and `$$...$$` display math render via `txm` to ANSI-styled Unicode art.
- **Code diff beautification** — centered line numbers, character-level diff highlighting within changed lines, and same-row pairing for side-by-side diffs.
- **CJK/emoji correctness** — a new `width` module provides grapheme-aware display width, truncation, and padding. All TUI rendering paths (markdown, input, sidebar, task panel, output, mermaid) consolidated onto this module, fixing ZWJ emoji splitting, CJK column overflow, and cursor misalignment.

---

### ✨ Features

#### Mermaid Diagrams

- **18 diagram types** — flowchart (`graph`/`flowchart` TD/LR/RL/BT), `sequenceDiagram`, `stateDiagram-v2`, `pie`, `gantt`, `timeline`, `erDiagram`, `classDiagram`, `mindmap`, `gitGraph`, `journey`, `quadrantChart`, `xychart-beta`, `block-beta`, `packet-beta`, `sankey-beta`, `requirementDiagram`, `architecture-beta`.
- **First-class output type** — `OutputItem::MermaidDiagram { source }` flows through `StreamFrame` → `Event` → `TranscriptEntry` → restore. Mermaid source is intercepted before reaching the markdown renderer, so diagrams render as interactive panels, not raw text.
- **Floating panel** — clicking the `⤢` button on a mermaid preview opens a floating panel with the full diagram. Panel supports vertical/horizontal scroll and Tab toggle between source and diagram split view.
- **Layout engine** — Sugiyama layered layout with predecessor-based layer assignment (sources at top), barycenter crossing reduction, and edge routing through layer gaps. Edges never cross node interiors.
- **CJK/emoji-aware grid** — the grid canvas uses display-width-aware cell allocation so wide characters (Chinese, Japanese, Korean, emoji) align correctly in node labels and edge labels.

#### Code Rendering

- **ANSI code blocks** — `parse_ansi_to_screen` converts VT100 escape sequences to a `TerminalScreen`, then `highlight_ansi` renders colored `Line`s. Markdown dispatches `lang == "ansi" || "terminal"` to this path.
- **Math formulas** — `pulldown-cmark` `ENABLE_MATH` option emits `InlineMath`/`DisplayMath` events. `render_math(tex)` calls `txm::render` → ANSI string → `highlight_ansi` → centered block. Both inline and display math render as centered blocks with blank-line padding.
- **Diff beautification** — line numbers are centered with a single-space gap (no separators). Character-level diff highlights changed characters within modified lines. Same-row pairing aligns delete/insert lines side-by-side.

#### Search

- **Search UX polish** — `Ctrl+K` opens the history search modal with a simplified title (`Search History · {scope}`), a fixed bottom help bar (`Enter=search  ↑↓/jk=navigate  Tab=scope  Esc=close  regex supported`), and an enhanced empty state with usage examples (`error`, `role:user`, `/regex/`).
- **Cheatsheet section** — the F1 cheatsheet now includes a "Search History (Ctrl+K)" section listing in-modal shortcuts.

#### Panels

- **Cheatsheet as floating panel** — F1 opens a `PanelKind::Cheatsheet` floating panel instead of a fixed-height modal. The panel is scrollable, draggable, and maximizable, so the full keybinding list is always visible regardless of terminal size.
- **Panel data source unification** — a single `Task(TaskKind)` variant replaces per-kind panel kinds. All panels (terminal, bash, workflow, agent) share one data path, eliminating the cache-key collision that made different panels show the same content.
- **Panel button redesign** — macOS traffic-light style buttons (close/minimize/maximize) with hover effects. Buttons have consistent padding in all modes.
- **Panel interactions** — ESC minimizes the focused panel. Double-click the titlebar to toggle maximize. Maximized panels restore to their previous size on unmaximize.

#### Terminal/Bash

- **Bash log restore** — bash task output restores from `log_path` when the `output` field is absent. `bash.status` and `bash.output` return `log_path` for reconstruction.
- **UUID handles** — bash/terminal handles use UUIDs instead of an incrementing counter, preventing ID collisions across restarts.
- **`[out]`/`[err]` prefix stripping** — restored bash output strips log prefixes for clean display.

---

### 🐛 Bug Fixes

#### CJK/Emoji

- **Grapheme-aware wrapping** — `wrap_text` in markdown and input editor now uses `width::graphemes` instead of `chars()`, fixing ZWJ emoji splitting and CJK column overflow.
- **Cursor misalignment** — input editor cursor positioning with wide characters fixed via grapheme-aware width calculation.
- **Sidebar/status truncation** — local `truncate` functions deleted; all paths use `width::truncate`.
- **Mermaid grid alignment** — wide characters in node labels and edge labels get padding cells so box-drawing characters align.

#### Mermaid

- **Layer assignment** — replaced `while changed` loop with DFS cycle detection + predecessor-based layer assignment. Sources are at layer 0 (top), sinks at the bottom. Cyclic graphs no longer infinite-loop.
- **Edge routing** — edges route through layer gaps, never through node interiors. Corner direction inversion fixed. Direct edge arrows no longer overwrite Corner turn characters. LR (left-right) edges use `SideChannel` path with branch edges from node top/bottom.
- **Box-drawing merge** — `merge_chars` uses a bitmask covering all 144 box-drawing character combinations, preventing garbled junctions.
- **Horizontal scroll** — CJK characters in scrolled mermaid views no longer corrupt. Scroll lines are padded to visible width.

#### Panels

- **Cache-key collision** — opening different panels from History all showed the same content. Fixed by matching `WorkflowPanel` by `run_id` instead of `rposition`.
- **`⤢` button region** — hardcoded `panel_item_index=0` replaced with the actual item index.
- **Maximize state** — maximized panels now exit maximize on drag/resize start, preventing jump glitches. `unmaximize` is delayed until drag/resize actually moves.
- **Panel size restore** — maximized panels restore to default size (88×29 → 80×24 content) on unmaximize, not to the maximized size.
- **Workflow panel** — renders the graph even when `task_snapshots` is empty. `workflow_panel_task_handle` returns the Flow `run_id`, not the last child node id.
- **Orphan panels** — session restore no longer creates orphan workflow panels that caused 100% CPU.

#### Markdown

- **Inline code wrapping** — backticks stripped from inline code; long inline code wraps at content width.
- **`&&` → `&` regression** — `clippy --fix` had corrupted boolean ops in `wrap_text` trailing-space trim. Fixed.

---

### ♻️ Refactors

- **`width` module** — new `crates/atman-tui/src/width.rs` providing `width(s)`, `graphemes(s)`, `truncate_plain(s, max_w)`, `truncate(s, max_w)`, `pad_right(s, target_w)`. All local `truncate`/`clamp_len`/`take_display_*`/`truncate_spans_to_width` functions deleted across `output.rs`, `sidebar.rs`, `status.rs`, `task_panel.rs`, `approval_bar.rs`, `floating_panels.rs`, `markdown.rs`, `input.rs`.
- **Mermaid Grid** — Grid rewritten to separate structure/text/border layers with `merge_chars` bitmask. `LayoutResult`/`LaidOutNode`/`LaidOutEdge`/`EdgePath` enums replaced `PositionedNode`.
- **Panel system** — `workflow_viewer_modal` and `terminal_viewer_modal` deleted. Fullscreen viewers are now maximized floating panels. `PanelKind` enum extended with `Mermaid` and `Cheatsheet` variants.

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
