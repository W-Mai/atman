# Atman Features Overview

## Core

- **Flows** — Declarative `.at` files define agent workflows as functions with typed params, contracts, and return values.
- **LLM calls** — `llm { model, messages, schema, tools, cache, retry }` node with prompt caching, retry, and structured output via schema types.
- **Tool dispatch** — Tools are called as `tool.name(args)` in flows. The agent loop extracts tool_uses from LLM responses and dispatches them via `dispatch_all`.
- **Subflows** — `subflow(name, args)` enables recursion and delegation. Each subflow gets its own turn context.

## Memory

- **Goal** — Persistent session goal injected as system prompt prefix on every LLM call.
- **Todo** — Short-lived execution items with `where/why/how/expected_result` fields.
- **Plan** — High-level ordered checklist injected into every LLM call.
- **Confession** — Records rule violations with auto-filled anchors (turn/flow_run/event_seq).
- **Spec** — Feature-level progress tracking (phase, updates, deviations).
- **History** — Full-text search (FTS5) across session messages, paginated read.

## Safety & Trust

- **Safety** — Content classification on LLM input/output. Modes: `warn` (log), `deny` (block). Optional auto-rewrite.
- **Trust modes** — `calm` (Tier::Zero auto), `steady` (Tier::One auto), `eager` (Tier::Two auto), `reckless` (all auto).
- **Outside behavior** — In Eager mode: `deny`, `approve`, or `allow` for tools outside the declared list.
- **Approval queue** — Pending tool calls await user decision (1-9, a=all, d=deny, Esc=deny all+cancel).

## Compaction

- Context compaction summarizes old messages to fit context window.
- Review modal: `always` / `manual-only` (default) / `never`.
- `find_compact_range` + `replace_messages_range` tools implement the compaction loop.

## Injection

- L1-L4 interjection levels: `!nudge` (L1), `!course-correct` (L2), `!redirect` (L3), `!stop` (L4).
- Injections queue to the next chunk boundary.

## Hunk Review

- `hunk.plan_edit` computes a diff-based EditProposal.
- `hunk.review` presents it to a human reviewer.
- `hunk.apply` writes selected hunks to disk.

## MCP (Model Context Protocol)

- Connect external tool servers via stdio/http/sse.
- Tools registered as `mcp.{server}.{tool}` in the ToolRegistry.
- `"mcp.*"` in a flow's tool list includes all MCP tools.
- Hot-reload: toggle/remove servers without restart.

## Lifecycle Hooks

- `on session.start` / `session.end` / `session.context_compact`
- `on turn.start` / `turn.end`

## Watch

- Runtime monitoring of LLM output: token pattern matching, elapsed time, token consumption.
- Actions: `abort(msg?)` stops the flow, `warn(msg?)` notifies the user.

## Provider System

- Multi-provider: Anthropic, OpenAI-compatible, Codex, GLM.
- Model aliases: `cheap`, `smart`, `smarter`, `codex`.
- Per-provider auth, context budget, max tokens, thinking mode.
