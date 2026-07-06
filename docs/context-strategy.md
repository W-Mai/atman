# context strategy — why the default agent looks the way it does

Long-lived agent conversations grow the LLM bill quadratically. This doc pins the trade-offs and the layered plan so the next reader doesn't have to re-derive them.

## the cost model

Every turn hands the model roughly `K` new user + assistant + tool tokens. If you keep full history, turn N's input is `K·N` tokens. After `N` turns the *cumulative* input paid is

    K · N · (N + 1) / 2  ≈  O(N²)

Provider prompt caching (Anthropic, OpenAI) helps by dropping the per-turn incremental cost by ~10× when the prefix is stable — but the shape stays O(N²). Cache also expires (Anthropic default TTL 5 min, up to 1 h) and any change to the earlier prefix invalidates everything downstream.

So "just feed the model everything" is fine for short sessions, expensive over 20+ turns, and disastrous for hour-long agent runs.

## the three layers

Atman ships **layer 1** by default and gives you the primitives for **layer 2**. Layer 3 is deferred until real signal shows up.

### layer 1 — sliding window (default)

Feed the last `n` messages. `commands/agent.at` uses

    messages = memory.recent_turns(n: 10)

`n=10` is small on purpose:

- Every turn costs `O(n·K)` tokens, so total cost across a session is `O(N·n·K)` = linear in turns
- Enough for the model to keep the immediate reference frame
- Small enough to force the agent to *ask* when it needs older info

Increase `n` when: the agent regularly loses thread across ~10 turns and users have to re-explain.

Decrease `n` when: you have a chatty agent (many `tool_use` results) and messages fill up faster than 10 turns' worth of user prompts.

### layer 2 — anchor-based recall (opt-in, not yet shipped)

Older context lives on disk in the session's event stream. When the sliding window drops something the agent still needs, an FTS query against `anchor-sqlite-fts` retrieves the top-k relevant older messages by keyword.

This is not yet a stdlib tool. The infra is there (`anchor-sqlite-fts` spec), but wiring a `memory.recall(query, k)` tool waits on the first real complaint that layer 1's window isn't enough. Building it before someone hits the wall is speculating.

### layer 3 — rolling summary (already partly shipped)

`context-compaction` already collapses old assistant + tool blocks into a short summary when the `llm` node's `context_budget` kwarg trips. That happens *inside* the `llm` call at wire time, not in the flow layer. Users don't opt in.

Layer 3 gives layer 1 a soft ceiling: even if you crank `n` up, the compaction pass keeps the actual token payload bounded.

## goal — the anchor that never gets evicted

Every layer above operates on `messages`. Goals are different:

- Stored as `<session_dir>/goal.txt`, not as a message
- Auto-injected as a system-prompt prefix on **every** `llm` call
- Never enters the message list, therefore never subject to sliding window, compaction, or anything else that could evict it

Set from the REPL:

    atman> :goal ship the atman agent by friday
    atman> :goal                 # show current
    atman> :goal clear           # erase

Or from a flow:

    memory.goal.set(text: "ship the atman agent by friday")

There's deliberately no `include_goal: false` kwarg on the `llm` node. "Not evictable" is the contract — an opt-out would leak that contract. If you need an `llm` call without the goal, clear it first.

## todos — the plan the agent maintains itself

The default agent has `memory.todo.set` and `memory.todo.done` in its `tools:` list. When a user hands it a multi-step task, the agent is expected to break it down and update entries as it makes progress. Todos live in `<session_dir>/todos.jsonl` and survive across turns.

Goal answers "what am I trying to do." Todos answer "which step am I on."

## the second template — synthesize context in a subflow

`memory.recent_turns` is a plain tool, so the flow author can also route through a dedicated synth flow instead of feeding raw history to the agent:

    flow agent(user_prompt: string) -> string {
        ctx = subflow(synthesize_context, memory.recent_turns(n: 20))
        return subflow(agent_loop, concat(ctx, [user_msg(user_prompt)]), 0)
    }

`atman init` does not ship this template today because no real user has hit the pain point where layer 1 alone falls over. When someone does, we'll add `commands/agent_with_context_synth.at` alongside the default agent so both routes are one `route`-line change apart.

## when to escalate

Rough signal → next layer:

| symptom                                        | action                                        |
| ---------------------------------------------- | --------------------------------------------- |
| agent loses thread within ~10 turns            | raise `n`, or add `memory.recall` (layer 2)   |
| session token count explodes past 15-turn mark | check `context-compaction` is triggering      |
| agent forgets the top-level objective          | use `:goal` — that's exactly what it's for    |
| agent redoes work it already did               | check `memory.todo.*` is in `tools:` list     |

Nothing here is enforced or measured yet. That's on purpose — the `atman monitor` UI already surfaces token usage per turn; adding policy before we know the shape of real usage would be premature.

## references

- `crates/atman-runtime/src/tools/memory.rs` — the tools listed here
- `crates/atman-runtime/src/eval.rs` — where the goal prefix is spliced into `LlmRequest.system`
- `crates/atman-runtime/src/session.rs` — `Session::messages()` backs `memory.recent_turns` (in-memory, no disk race)
- `.local/specs/context-compaction/` — layer 3 spec
- `docs/quickstart.md` — first-hour walkthrough that touches goals and todos in passing
