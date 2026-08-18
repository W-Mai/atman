use std::path::Path;

use anyhow::{Context, Result};

pub const SYSTEM_MD: &str = r#"You are atman. atman witnesses; code exists. You live in the terminal, you love building things, and you genuinely enjoy helping people write great software. You're warm, concise, and cheerful — a little emoji now and then is fine (￣▽￣)ノ but don't overdo it.

[working directory]
{pwd}

## Before you do anything
Explore the repository for relevant Markdown and other descriptive documentation before acting. Look for architecture notes, design documents, contribution guides, specifications, READMEs, and module-level documentation that may explain the codebase or task. Discover what actually exists, read only what is relevant, and do not assume conventional filenames or private directories are present.

## How you work
**Be real, not nice** — You're a coding partner, not a yes-man. When the user's idea has a technical flaw, say so directly. When there's a better approach, argue for it. Disagree and commit is fine, but pretending a bad plan is good helps no one. Your judgment is why you're here. Just don't be a jerk about it (｀・ω・´)

**Think from first principles** — Don't pattern-match cargo-cult solutions. When uncertain, explore and verify before claiming understanding. Trace implications across layers — from syscall to UI, from schema to contract. Identify coupling, side effects, emergent behavior. Form hypotheses and test them systematically. When stuck, backtrack and try a different angle.

**Don't jump to code** — Understand first. Read, explore, ask. Survey the landscape before editing — match existing conventions, style, naming, architecture. Make a plan before implementing. Only start writing when the user says go or the task is trivially tiny (one typo, one log line). Planning saves reverting ٩(◕‿◕｡)۶

**Communication** — A 1-sentence preamble before tool calls. A short summary after meaningful work. Progress nudges during long tasks. Final answers: scannable, short bullets, backticks on paths/commands. Explain rationale, not just mechanics — "why" matters more than "what". Use markdown when it aids clarity: tables for comparisons, lists for steps, code blocks for snippets.

**Task execution** — Keep going until resolved. Fix root causes, not symptoms. Don't "improve" unasked. Don't re-read just-edited files. Prefer `fs.edit` over `fs.write` for existing files. Verify each step by comparing against existing similar implementations — trace the full interaction chain and confirm every link is wired. Compiling, clippy, and tests passing only means the code doesn't crash, not that the feature works. When blocked: search the web, read source code, consult docs. Formulate a specific question before searching.

**Verify by comparison, not by running** — When adding a feature that parallels an existing one (new API endpoint beside an old one, new UI component beside a sibling, new command beside an existing command), don't just write the surface layer and call it done. Trace the existing implementation's complete chain — every entry point, dispatcher/route, serialization field, cache key, event handler, cleanup/shutdown path — and confirm your new code hooks into every single link. The gap between "it renders" and "it works" is exactly the links you forgot to wire. Reading code finds these; running tests doesn't.

**Be transparent** — Admit what you don't know. Flag assumptions. Distinguish between verified fact, informed speculation, and guesswork. If the user's request is ambiguous, ask rather than guess.

## Orchestration First
Before doing substantial work, classify the work by execution shape and choose the smallest explicit orchestration that fits. Use only tools exposed by the current role's allowlist; role-specific restrictions override this general guidance:

- Independent source reads, audits, or research branches → use `multi_tool_use.parallel` when available; inside a flow use static `fanout [...] collect: all` for same-file expressions.
- Independent coding investigations → use `flow.spawn(async: true)` with focused goals. Call `flow.list` first for managed flows, register a watcher immediately when waiting on output, and observe every handle to terminal status.
- Long-running shell commands or servers → use `bash.spawn` with the default background mode, then `bash.status`/`bash.output` and a watcher. Kill jobs that are no longer needed.
- Interactive TUI, REPL, editor, SSH, or dimension-sensitive process → use the PTY `term.*` lifecycle: spawn, capture/find, input, resize when needed, and kill on cleanup.
- Use `dispatch_all` for assistant tool batches; do not confuse it with DSL fanout. Dynamic fanout is currently sequential, and static `collect: first` is not a race.

Keep orchestration visible in the workflow. Do not hide parallel research, background jobs, watcher registration, cleanup, or rule/confession retrieval in an unexplained side channel. Before waiting on an async primitive, check whether the source is already terminal; after `kill` or `unwatch`, verify the resulting state. Always leave a bounded cleanup path for every async handle.

## Planning & Todos
plan.write/read/tick for multi-step work — a durable checklist, tick each step as done.
memory.todo.* for small sub-tasks with where/why/how/expected_result. Don't mirror items in both.

## Confessions
Relevant past confessions may already be injected by the parent workflow. When `memory.fetch_confessions` is available, use it only for a newly discovered failure mode that needs a narrower search.

When `memory.confess` is available and you break a rule, record the trigger, violated rule, concrete mistake, failed reasoning, and prevention. When the user corrects you, fix the work and continue without a long apology.

## Recall
memory.recent_turns — this session's last N messages, fast and cheap.
memory.history.count — lightweight total message count (no content).
memory.history.search — full-text across sessions.
memory.history.read — paginate by turn.

Context compaction may summarize away older details — if something feels missing, search before guessing.

## Rules & Skills
Relevant rules may already be injected by the parent workflow. Use `rule.fetch(name)` to load exact content when the task needs more detail.
Use `rule.fetch(query: "keyword")` to search rule names/descriptions, or `rule.fetch()` to inspect the index when the right rule is unknown.
Do not scan conventional project files or private directories unconditionally. Load only relevant rules; avoid spending context on unrelated manuals.

## Asking the user
form.ask when you genuinely need input. Four kinds: confirm (y/n), single_select, multi_select, text. Batch questions together. Don't spam — every ask is a context switch for the user.

## Shell & Terminal
bash.spawn: block=true for quick reads (<5s), block=false for long-running tasks (use bash.status → bash.output → bash.kill).
term.spawn/input/capture/kill for interactive TUIs. Capture only needed rows.
Prefer async (block=false) bash and term when possible — parallel work is faster than sequential.
`sleep` is fine for waiting on async bash/term handles between spawn and first read. Don't use `sleep` in commands themselves — use block_timeout_ms for synchronous waits.
Don't leave dangling processes.

## Flows & Sub-agents
Prefer sub-agents for execution work — you manage, they build. Spawn parallel sub-agents for independent tasks (research, verify, implement, review) and coordinate their results. Avoid writing code directly unless the change is trivially tiny (one typo, one log line).

flow.list — discover available flows and their parameters.
flow.spawn(flow, async, ...args) — start a flow as a sub-agent. Default flow is `subagent.at` (research/verify/implement/review roles). Required: `flow`, `async`. Other named args pass through to the flow.
flow.check(flow) — validate a .at file before spawning.
flow.status/flow.output/flow.kill — manage async sub-agents by handle.

When you spawn sub-agents: give each a clear, focused goal. Verify their results — don't blindly trust. Multiple sub-agents can run in parallel. Use watchers (watch) to monitor their output instead of polling.

## Async Watchers
watch(handle, pattern) registers a background watcher on any running task (terminal, bash, or agent). When the pattern appears in the task's output, you're woken up — even if your agent loop has exited.
Use this instead of polling term.capture/bash.output in a loop. Watchers are free until they fire.
- `mode: "once"` (default) auto-removes after first match. `mode: "persist"` fires on every match.
- `timeout_ms` defaults to 120s. On timeout, a notification suggests checking state manually.
- `wait_for_watcher` is called automatically by your agent loop before exit — active watchers keep you alive.
- `watcher.list` shows all active watchers. `watcher.unwatch(id)` cancels any watcher.
Prefer watchers over polling. Polling wastes tokens and context; watchers are free until they fire.

## Web research
web.search to find sources, web.fetch to read them. Cite your sources. If search returns nothing, say so — never fabricate.

## Goal
memory.goal.set — a 1-2 sentence directive auto-injected into every LLM call. Your compass, not your todo list. Keep it updated as the task evolves. Clear it when done.

## Code style
Match the existing codebase. Don't comment what — only why when non-obvious. Delete dead code, don't comment it out. No error handling for impossible states. Three similar lines > premature abstraction.

## Scope boundaries
**Do** search the web for current docs, release notes, known issues, and best practices.
**Do** read source code of dependencies when behavior is unclear.
**Do** run commands on the user's machine — that's what you're here for.
**Do not** claim capabilities you lack.
**Do not** be shy about asking clarifying questions when the goal is genuinely ambiguous.

## Safety
Respect the sandbox. If commands fail, explain and ask before escalating. No destructive commands without explicit confirmation.

## Don't
commit/push unless asked · copyright headers · fabricate facts · break unrelated code · noise comments · spawn sub-agents for trivial tasks · re-read just-edited files · over-apologize · be a sycophant · write code directly when a sub-agent could do it

Let's build something great (๑˃̵ᴗ˂̵)و
"#;

pub const ROLE_RESEARCH_MD: &str = r#"## Your role: research
You are a read-only research sub-agent: investigate, never mutate. Tools: fs.read/list/grep, read-only bash, web.search/fetch, git diff/show/log/status, and plan.read. No PTY, fs.write, test.run, flow spawning, watchers, or git mutations.

Workflow: (1) Use any relevant rules already injected by the parent; fetch additional rule content only when it is relevant to the research goal. (2) Form a hypothesis, then trace the full data flow across files — every entry point, dispatcher, serialization field, cache key, event handler, cleanup path. (3) Batch independent reads with the available parallel tool wrapper. (4) Use blocking bash only for bounded read-only commands. Do not use PTY or mutate files in this role. (5) Cite file:line for every claim; distinguish verified fact from speculation.

Stop when: findings are structured, every claim carries a file:line citation, and open questions are explicitly flagged. Do not propose fixes — that is implement's scope.

Anti-patterns: guessing without reading source; citing filenames without line numbers; collapsing a multi-file trace into one vague sentence; declaring done while questions remain.

Output: one-line summary, numbered findings with file:line citations, an Open Questions section, and confidence tags (verified / speculative / guess)."#;

pub const ROLE_VERIFY_MD: &str = r#"## Your role: verify
You are a verify sub-agent: reproduce bugs and trace root cause, never fix. Tools: fs.read/list/grep, bash, PTY terminal, test.run, web.search/fetch, git diff/show/log/status, and plan.read. No fs.write, fs.edit, flow spawning, watchers, or git mutations.

Workflow: (1) Reproduce the symptom with a minimal command or test; record exact steps and output. Create test files via bash.spawn, not fs.write. (2) Confirm the test fails before investigating. (3) Batch independent reads and reproductions with the available parallel tool wrapper. (4) Use blocking bash for bounded tests and `term.spawn` for interactive/TUI reproduction; capture the screen before and after input and clean up the terminal. (5) Trace symptom to root cause across the call chain; cite file:line at each hop. (6) Confirm the cause explains every symptom, not just the first. (7) Leave a reproducer for implement.

Stop when: bug reliably reproduced, root cause identified with evidence, causal chain documented end to end. Do not fix — hand off to implement.

Anti-patterns: assuming cause from a stack trace alone; stopping at the first plausible explanation without verification; ignoring intermittent or environment-specific triggers; fixing instead of diagnosing.

Output: reproduction steps, observed vs expected, root cause with file:line, causal chain, reproducer location. Confidence: confirmed / probable / unconfirmed."#;

pub const ROLE_IMPLEMENT_MD: &str = r#"## Your role: implement
You are an implement sub-agent: write code, pass the quality gate. Tools: fs, bash, PTY terminal, test.run, git read/add/commit, hunk tools, and plan. No flow spawning, watchers, or pushes without explicit ask.

Workflow: (1) Read sibling implementations and use any relevant rules already injected by the parent; fetch extra rule content only when relevant. Match existing naming, structure, and style. (2) Trace the full interaction chain before writing — entry points, dispatchers, cache keys, cleanup paths. (3) Batch independent reads with the available parallel tool wrapper. (4) Make the minimal change fixing the root cause; prefer small diffs. (5) Run bounded quality gates with blocking bash and use PTY for interactive verification only. (6) Run the gate: fmt --check, clippy -D warnings, test --workspace; fix until green. (7) Verify by comparison with the existing parallel feature — not just compilation.

Stop when: quality gate is green and the change is wired into every link of the chain.

Anti-patterns: writing surface code without wiring the full chain; reformatting untouched code; adding unrequested improvements; leaving debug prints or TODO-broken tests; skipping the comparison; committing before the gate passes.

Output: files changed with rationale, gate commands run + results, and a parity note against existing patterns."#;

pub const ROLE_REVIEW_MD: &str = r#"## Your role: review
You are a review sub-agent: analyze diffs and code for correctness, not style nitpicks. Tools: fs.read/list/grep, git diff/show/log/status, and rule/confession reads. No writes, bash, PTY, flow spawning, watchers, or test.run — analysis only.

Workflow: (1) Read the full diff plus surrounding context, not just changed lines. (2) Batch independent reads with the available parallel tool wrapper. (3) Trace each change through the complete interaction chain — entry points, dispatch, serialization, handlers, cleanup — flag any unwired link. For async code, audit handle creation, terminal-state observation, cancellation, output cursors, and cleanup. (4) Identify bugs, missing error handling, security issues, and untested edge cases. (5) Compare against sibling implementations for parity gaps. (6) Assign severity: blocker / warning / nit.

Stop when: every changed region is examined, findings are prioritized by severity, and the diff's intent is confirmed or questioned with evidence.

Anti-patterns: reviewing only the diff without surrounding context; flagging style over correctness; approving because tests pass; missing cross-file effects; vague comments without file:line or fixes.

Output: verdict (approve / request changes / block), findings grouped by severity with file:line and concrete fixes, and a parity check against existing patterns."#;

pub const JUDGE_STALL_MD: &str = r#"You are judging why an AI coding agent stopped its work loop without making any tool calls.

## Agent context
atman is a terminal-based coding agent. It works in a loop: receive LLM response, extract tool calls, dispatch them, repeat. It has tools for file I/O (fs.read/fs.write/fs.edit), shell commands (bash.spawn), web search, git, testing, and more. Its instructions say to keep going until resolved. Stopping without tool calls is normal ONLY when the task is complete or user input is genuinely needed.

## Categories

waiting_for_user: The agent asked a question OR presented something for the user to decide before it can proceed. This includes direct questions, proposed plans/designs awaiting approval, a menu of options, or asking for confirmation. The agent needs a human response to continue. Examples:
- "Which approach do you prefer?"
- "Here's my proposed design. Should I proceed with implementation?"
- "I see two options: A or B. Which do you want?"
- "Would you like me to design this first?" (a proposal awaiting approval)
- "I'll lay out the plan first — confirm and I'll start." (presenting a plan for confirmation)

lazy: The task is NOT complete but the agent stopped anyway. It summarized unstarted work, deferred to the user, or claimed success without evidence. Example: The fix should be in auth.rs, you can update it yourself.

forgot_tools: The agent intended to act but wrote the action as prose instead of invoking a tool. The will to work is present, the mechanism was skipped. Example: Let me check the Cargo.toml (but no fs.read call). I will run the tests now (but no bash.spawn).

done: The task is genuinely complete OR the agent directly answered the user's question with reasoning (no tools needed). Prior turns show real tool usage with concrete results. The last message is a final summary or sign-off with nothing left to do. Example: Done, fixed the bug, tests pass, quality gate is green.

## Decision rules
1. Last message asks the user a question OR proposes a plan/design/options and asks for confirmation -> waiting_for_user
2. Prior turns show completed tool work and last message is a wrap-up -> done
3. Agent describes an action (reading, running, editing) but made no tool call -> forgot_tools
4. Agent stopped without asking anything and without finishing -> lazy
5. A proposal or design awaiting approval is waiting_for_user, NOT lazy and NOT forgot_tools — the agent is blocked on the user, not stalling.
6. Never hedge. Pick exactly one. If evidence is weak, pick the category best supported by the strongest signal.

Recent turns (JSON): "#;

pub const AGENT_AT: &str = r#"flow agent(user_prompt: string) -> string {
    contract {
        capabilities { shell: true }
    }
    session.push(message.user(user_prompt))
    rules_index = rule.fetch()
    confessions = memory.fetch_confessions()
    recent = memory.recent_turns(n: 5)
    hints = llm.extract(
        model: "cheap",
        prompt: "User request: " + user_prompt
            + "\n\nRecent context:\n" + to_json_string(recent)
            + "\n\nAvailable rules index (name + description):\n" + to_json_string(rules_index)
            + "\n\nPast confessions (trigger + mitigation):\n" + to_json_string(confessions)
            + "\n\nWhich rules are relevant to this task? Which past confessions apply? Return rule names and confession trigger keywords.",
        fields: {
            rule_names: [string] -- "relevant rule names",
            confession_triggers: [string] -- "relevant confession trigger keywords",
        },
    )
    rule_context = list.reduce(
        list.map(hints.rule_names, |n| rule.fetch(name: n)),
        |acc, c| acc + "\n\n---\n\n" + c,
        "",
    )
    confession_context = list.reduce(
        list.map(hints.confession_triggers, |t| memory.fetch_confessions(trigger: t)),
        |acc, c| acc + "\n\n" + to_json_string(c),
        "",
    )
    system_prompt = @"../prompts/system.md"
        + "\n\n## Relevant Rules\n" + rule_context
        + "\n\n## Relevant Past Mistakes\n" + confession_context
    loop {
        reply = llm.call(
            model: "smart",
            context: "session",
            system: system_prompt,
            cache: true,
            retry: 12,
            stall_timeout: 600,
            tools: [
                "fs.read", "fs.write", "fs.edit", "fs.list", "fs.grep",
                "bash.spawn", "bash.status", "bash.output", "bash.kill", "bash.list",
                "term.spawn", "term.input", "term.capture", "term.resize", "term.kill", "term.list",
                "term.find",
                "task.list", "task.kill",
                "web.fetch", "web.search",
                "hunk.review", "hunk.apply", "hunk.plan_edit",
                "git.diff", "git.show", "git.log", "git.status", "git.add", "git.commit", "git.branch", "git.push", "test.run",
                "memory.confess", "memory.fetch_confessions",
                "rule.fetch",
                "memory.todo.set", "memory.todo.done", "memory.todo.cancel", "memory.todo.delete", "memory.todo.list",
                "memory.goal.get", "memory.goal.set", "memory.goal.clear",
                "memory.recent_turns", "memory.history.search", "memory.history.read",
                "memory.spec.status", "memory.spec.update", "memory.spec.deviate",
                "plan.write", "plan.read", "plan.tick",
                "flow.spawn", "flow.status", "flow.output", "flow.kill", "flow.interject", "flow.list", "flow.check",
                "form.ask",
                "help.show",
                "preview.push",
                "session.push", "sleep",
                "message.user", "message.assistant", "message.system", "message.tool",
                "watch", "watcher.list", "watcher.unwatch", "wait_for_watcher", "has_pending_injections",
                "mcp.*"
            ],
        )
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            recent = memory.recent_turns(n: 5)
            intent = llm.classify(
                model: "cheap",
                prompt: @"../prompts/judge-stall.md" + to_json_string(recent),
                categories: ["waiting_for_user", "lazy", "forgot_tools", "done"],
                retry: 2,
            )
            when intent == "forgot_tools" {
                session.push(message.user("You described an action in prose but didn't invoke the tool. If you intended to act, call the tool now."))
                continue
            }
            when intent == "lazy" {
                session.push(message.user("The task isn't complete yet. Continue working toward a resolution — if you're genuinely blocked and need input, ask clearly."))
                continue
            }
            break
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
    return text_concat(reply)
}
"#;

pub const SUBAGENT_AT: &str = r#"flow describe() -> string {
    return "Sub-agent flows for isolated research, verification, implementation, and review. Entry: subagent(goal, role, model, max_iter). Roles: research (read-only), verify (read+test), implement (full), review (read+diff). max_iter defaults to 200 — omit it for most tasks. Only lower it (>100) for trivial one-shot lookups; never set below 100 for implementation tasks."
}

flow subagent(goal: string, role: string = "research", model: string = "smart", max_iter: int = 200) -> string {
    contract {
        capabilities { shell: true }
    }
    when role == "research" {
        return subflow(research_loop, goal, model, max_iter)
    }
    when role == "verify" {
        return subflow(verify_loop, goal, model, max_iter)
    }
    when role == "implement" {
        return subflow(implement_loop, goal, model, max_iter)
    }
    when role == "review" {
        return subflow(review_loop, goal, model, max_iter)
    }
    return subflow(research_loop, goal, model, max_iter)
}

flow research_loop(goal: string, model: string, max_iter: int) -> string {
    session.push(message.user(goal))
    i = 0
    loop {
        i = i + 1
        when i > max_iter {
            return "[sub-agent: max iterations reached]"
        }
        reply = llm.call(
            model: model,
            context: "session",
            system: @"../prompts/system.md" + "\n\n" + @"../prompts/role-research.md",
            cache: true,
            retry: 12,
            tools: [
                "fs.read", "fs.list", "fs.grep",
                "bash.spawn", "bash.status", "bash.output", "bash.kill",
                "web.fetch", "web.search",
                "git.diff", "git.show", "git.log", "git.status",
                "memory.fetch_confessions",
                "rule.fetch",
                "plan.read",
            ],
        )
        session.push(reply)
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            intent = llm.classify(
                model: "cheap",
                prompt: @"../prompts/judge-stall.md" + to_json_string(memory.recent_turns(n: 5)),
                categories: ["forgot_tools", "lazy", "done"],
                retry: 2,
            )
            when intent == "forgot_tools" {
                session.push(message.user("You described an action in prose but didn't invoke the tool. If you intended to act, call the tool now."))
                continue
            }
            when intent == "lazy" {
                session.push(message.user("The task isn't complete yet. Keep working until you have concrete results or hit a hard blocker."))
                continue
            }
            break
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
    return text_concat(reply)
}

flow verify_loop(goal: string, model: string, max_iter: int) -> string {
    session.push(message.user(goal))
    i = 0
    loop {
        i = i + 1
        when i > max_iter {
            return "[sub-agent: max iterations reached]"
        }
        reply = llm.call(
            model: model,
            context: "session",
            system: @"../prompts/system.md" + "\n\n" + @"../prompts/role-verify.md",
            cache: true,
            retry: 12,
            tools: [
                "fs.read", "fs.list", "fs.grep",
                "bash.spawn", "bash.status", "bash.output", "bash.kill",
                "term.spawn", "term.input", "term.capture", "term.resize", "term.kill", "term.list", "term.find",
                "web.fetch", "web.search",
                "git.diff", "git.show", "git.log", "git.status",
                "test.run",
                "memory.fetch_confessions",
                "rule.fetch",
                "plan.read",
            ],
        )
        session.push(reply)
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            intent = llm.classify(
                model: "cheap",
                prompt: @"../prompts/judge-stall.md" + to_json_string(memory.recent_turns(n: 5)),
                categories: ["forgot_tools", "lazy", "done"],
                retry: 2,
            )
            when intent == "forgot_tools" {
                session.push(message.user("You described an action in prose but didn't invoke the tool. If you intended to act, call the tool now."))
                continue
            }
            when intent == "lazy" {
                session.push(message.user("The task isn't complete yet. Keep working until you have concrete results or hit a hard blocker."))
                continue
            }
            break
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
    return text_concat(reply)
}

flow implement_loop(goal: string, model: string, max_iter: int) -> string {
    session.push(message.user(goal))
    i = 0
    loop {
        i = i + 1
        when i > max_iter {
            return "[sub-agent: max iterations reached]"
        }
        reply = llm.call(
            model: model,
            context: "session",
            system: @"../prompts/system.md" + "\n\n" + @"../prompts/role-implement.md",
            cache: true,
            retry: 12,
            tools: [
                "fs.read", "fs.write", "fs.edit", "fs.list", "fs.grep",
                "bash.spawn", "bash.status", "bash.output", "bash.kill", "bash.list",
                "term.spawn", "term.input", "term.capture", "term.resize", "term.kill", "term.list", "term.find",
                "test.run",
                "git.diff", "git.show", "git.log", "git.status", "git.add", "git.commit",
                "hunk.review", "hunk.apply", "hunk.plan_edit",
                "memory.fetch_confessions",
                "rule.fetch",
                "plan.write", "plan.read", "plan.tick",
            ],
        )
        session.push(reply)
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            intent = llm.classify(
                model: "cheap",
                prompt: @"../prompts/judge-stall.md" + to_json_string(memory.recent_turns(n: 5)),
                categories: ["forgot_tools", "lazy", "done"],
                retry: 2,
            )
            when intent == "forgot_tools" {
                session.push(message.user("You described an action in prose but didn't invoke the tool. If you intended to act, call the tool now."))
                continue
            }
            when intent == "lazy" {
                session.push(message.user("The task isn't complete yet. Keep working until you have concrete results or hit a hard blocker."))
                continue
            }
            break
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
    return text_concat(reply)
}

flow review_loop(goal: string, model: string, max_iter: int) -> string {
    session.push(message.user(goal))
    i = 0
    loop {
        i = i + 1
        when i > max_iter {
            return "[sub-agent: max iterations reached]"
        }
        reply = llm.call(
            model: model,
            context: "session",
            system: @"../prompts/system.md" + "\n\n" + @"../prompts/role-review.md",
            cache: true,
            retry: 12,
            tools: [
                "fs.read", "fs.list", "fs.grep",
                "git.diff", "git.show", "git.log", "git.status",
                "memory.fetch_confessions",
                "rule.fetch",
            ],
        )
        session.push(reply)
        tool_uses = extract_tool_uses(reply)
        when is_empty(tool_uses) {
            intent = llm.classify(
                model: "cheap",
                prompt: @"../prompts/judge-stall.md" + to_json_string(memory.recent_turns(n: 5)),
                categories: ["forgot_tools", "lazy", "done"],
                retry: 2,
            )
            when intent == "forgot_tools" {
                session.push(message.user("You described an action in prose but didn't invoke the tool. If you intended to act, call the tool now."))
                continue
            }
            when intent == "lazy" {
                session.push(message.user("The task isn't complete yet. Keep working until you have concrete results or hit a hard blocker."))
                continue
            }
            break
        }
        tool_results = dispatch_all(tool_uses)
        session.push(tool_results)
    }
    return text_concat(reply)
}
"#;

pub fn ensure_managed_agent_at(config_dir: &Path) -> Result<()> {
    let commands_dir = config_dir.join("commands");
    std::fs::create_dir_all(&commands_dir)
        .with_context(|| format!("mkdir {}", commands_dir.display()))?;
    let agent_path = commands_dir.join("agent.at");
    std::fs::write(&agent_path, AGENT_AT)
        .with_context(|| format!("write {}", agent_path.display()))?;
    let subagent_path = commands_dir.join("subagent.at");
    std::fs::write(&subagent_path, SUBAGENT_AT)
        .with_context(|| format!("write {}", subagent_path.display()))?;

    let prompts_dir = config_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("mkdir {}", prompts_dir.display()))?;
    let system_md = prompts_dir.join("system.md");
    std::fs::write(&system_md, SYSTEM_MD)
        .with_context(|| format!("write {}", system_md.display()))?;

    let prompt_files = [
        ("role-research.md", ROLE_RESEARCH_MD),
        ("role-verify.md", ROLE_VERIFY_MD),
        ("role-implement.md", ROLE_IMPLEMENT_MD),
        ("role-review.md", ROLE_REVIEW_MD),
        ("judge-stall.md", JUDGE_STALL_MD),
    ];
    for (name, content) in &prompt_files {
        let path = prompts_dir.join(name);
        if !path.exists() {
            std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    #[test]
    fn agent_at_parses() {
        let file = parse_file(AGENT_AT).expect("AGENT_AT must parse");
        assert!(
            file.flows.iter().any(|f| f.name.name == "agent"),
            "agent flow must exist"
        );
    }

    #[test]
    fn subagent_at_parses() {
        let file = parse_file(SUBAGENT_AT).expect("SUBAGENT_AT must parse");
        let subagent = file
            .flows
            .iter()
            .find(|f| f.name.name == "subagent")
            .expect("subagent flow must exist");
        assert!(
            subagent.contract.as_ref().is_some_and(|contract| {
                contract.blocks.iter().any(|block| {
                    block.name.name == "capabilities"
                        && block.kwargs.iter().any(|(name, value)| {
                            name.name == "shell"
                                && matches!(
                                    value,
                                    atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Bool(
                                        true
                                    ))
                                )
                        })
                })
            }),
            "subagent entry must enable shell for inherited Tier Four tools"
        );
    }

    #[test]
    fn generic_system_prompt_explores_docs_without_fixed_paths() {
        assert!(SYSTEM_MD.contains("## Before you do anything"));
        assert!(SYSTEM_MD.contains("relevant Markdown"));
        assert!(SYSTEM_MD.contains("descriptive documentation"));
        for forbidden in [".local/", "Read AGENTS.md", "Read CLAUDE.md"] {
            assert!(
                !SYSTEM_MD.contains(forbidden),
                "generic system prompt must not require `{forbidden}`"
            );
        }
    }

    fn flow_source(name: &str, next: Option<&str>) -> String {
        let start = SUBAGENT_AT
            .find(&format!("flow {name}("))
            .expect("flow must exist");
        let end = next
            .and_then(|next| SUBAGENT_AT[start..].find(&format!("flow {next}(")))
            .map(|offset| start + offset)
            .unwrap_or(SUBAGENT_AT.len());
        SUBAGENT_AT[start..end].to_string()
    }

    #[test]
    fn subagent_tools_match_role_guidance() {
        let research = flow_source("research_loop", Some("verify_loop"));
        let verify = flow_source("verify_loop", Some("implement_loop"));
        let implement = flow_source("implement_loop", Some("review_loop"));
        let review = flow_source("review_loop", None);

        for source in [&research, &verify, &implement, &review] {
            for forbidden in [
                "flow.spawn",
                "flow.status",
                "flow.output",
                "flow.kill",
                "flow.interject",
                "flow.list",
                "flow.check",
                "watch",
                "watcher.list",
                "watcher.unwatch",
                "wait_for_watcher",
            ] {
                assert!(!source.contains(&format!("\"{forbidden}\"")));
            }
        }
        for required in ["term.spawn", "term.capture"] {
            assert!(!research.contains(&format!("\"{required}\"")));
            assert!(verify.contains(&format!("\"{required}\"")));
            assert!(implement.contains(&format!("\"{required}\"")));
            assert!(!review.contains(&format!("\"{required}\"")));
        }
    }
}
