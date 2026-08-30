# Atman DSL Syntax Reference

The parser and evaluator in `atman-dsl` and `atman-runtime` define the executable
DSL described here. Older examples using `llm { ... }` or `fetch_rule(...)` are
obsolete. Use `llm.call(...)` and `rule.fetch(...)`.

## Top-Level Declarations

| Syntax | Runtime behavior |
|---|---|
| `flow name(params) -> ret { ... }` | Declares a flow. Parameters require types and may have defaults. Types and return types are currently metadata. |
| `route "pattern" { flow: name }` | Routes matching user input to a flow. |
| `default_route { flow: name }` | Fallback route. |
| `on session.start { ... }` | Lifecycle hook. Supported events: `session.start`, `session.end`, `session.context_compact`, `turn.start`, `turn.end`. |

Lifecycle hooks are loaded from configured `.at` files. A flow file invoked directly
is not automatically a lifecycle registry, so put hooks in the configured commands
directory when they must run in CLI or daemon sessions.

## Statements

```atman
flow example(input: string) -> string {
    record = { request: input, ready: true }
    { request: question, ready } = record
    when ready {
        return question
    }
}
```

Supported statements are bindings, struct destructuring, `when`, `return`, bare
expressions, `watch`, unconditional `loop`, `break`, and `continue`.

There is no `if`, `else`, `for`, or `while`. Use `when`, a bounded recursive
subflow, or `loop` with an explicit `break`. Every unconditional loop needs a
termination path; a loop without `break`, `return`, error, or cancellation does
not converge.

## Expressions

### Literals and values

| Syntax | Value |
|---|---|
| `"..."`, `123`, `1.5`, `true` | String, integer, float, boolean |
| `@"path/to/file"` | File reference loaded as text where a string is expected |
| `[a, b]` | List |
| `{ field: value }` | Ordered struct of string/value fields |
| `record.field` | Struct member access |
| `|>`, `||`, `&&`, comparisons, arithmetic | Pipe, boolean, comparison, and arithmetic operators |

There is no map literal distinct from a struct and no `items[0]`/`map[key]` index
syntax. Use `head`, `tail`, `len`, list combinators, destructuring, or a tool that
returns the required shape.

### Lambdas and list combinators

Lambdas are expression-only and can capture their surrounding environment:

```atman
mapped = list.map(items, |item| item.name)
selected = list.filter(mapped, |name| name != "")
joined = list.reduce(selected, |acc, name| acc + "\n" + name, "")
```

The lambda forms `list.map`, `list.filter`, `list.find`, `list.any`, `list.all`,
and `list.reduce` execute sequentially. The older registered tools such as
`list_map` and `list_reduce` are separate APIs and are not lambda aliases.

## Tool Calls and LLM Calls

Every dotted call is a tool call. Tool names use `namespace.action` spelling.

```atman
reply = llm.call(
    model: "smart",
    context: "session",
    system: @"prompts/system.md",
    effort: env("effort"),
    retry: 3,
    stall_timeout: 120,
    tools: ["fs.read", "bash.spawn", "mcp.*"],
)
```

`env("name")` reads immutable data supplied for the current root invocation; it
does not read process environment variables and is not exposed as an LLM tool.
A missing key evaluates to unit. `effort: env("effort")` therefore uses the
input/CLI/daemon selection when present and otherwise leaves the call on its
model-configured default.

`llm.call`, `llm.extract`, `llm.classify`, and `llm.generate_branches` share this
explicit effort behavior. Each helper consumes the invocation value only when
its own call includes `effort: env("effort")`; inheriting the invocation
environment alone does not change an LLM request.

`llm.call` accepts `model`, `prompt`, `messages`, `system`, `input`, `context`,
`cache`, `retry`, `retry_classified`, `context_budget`, `stall_timeout`, `tools`,
`effort`, `reasoning`, `reasoning_budget`, `thinking`, and `fallback`. `effort`
and `reasoning` both accept `default`, `off`, `auto`, an effort such as `high`,
or an effort and execution mode such as `high@pro`; `effort` additionally treats
unit as absent for `env(...)` composition. `reasoning_budget` is a positive token
count for providers that expose budget-based thinking. Only one of `effort`,
`reasoning`, and `reasoning_budget` may be supplied. `thinking` remains a boolean
compatibility input. `fallback` is evaluated before the call; use an explicit
`when result.is_err()` path when the fallback itself has side effects.

Structured extraction is a normal tool call:

```atman
selection = llm.extract(
    model: "cheap",
    prompt: "Return only the requested structured fields.",
    fields: {
        rule_names: [string] -- "exact rule names",
        confession_triggers: [string] -- "search hints",
    },
    retry: 2,
)
```

`fields` must be a non-empty struct. Supported extraction types include `bool`,
`int`, `float`, `string`, `[string]`, and `[int]`.

## Subflows and Fanout

A same-file subflow call uses an identifier and positional or named arguments:

```atman
answer = subflow(worker, question, limit: 3)
```

Subflow calls create child flow-run events and inherit the parent evaluation context.
The target flow's own contract and default parameter values are not installed by
this path, so pass required values explicitly and put effective capabilities on
the parent flow.

### Parallelism decision table

| Need | Use | Important limitation |
|---|---|---|
| Run a fixed set of same-file expressions concurrently | `fanout [a, b] collect: all` | Preserves input order; any branch error fails the fanout. |
| Run one expression per item | `fanout source { |item| expr } collect: all` | Current evaluator is sequential despite the name. |
| Race fixed fanout branches | Not available | Static `collect: first` parses but is not implemented. |
| Batch assistant tool calls | `dispatch_all(tool_uses)` | Auto-approved calls can run in parallel; approval-gated calls are serialized after approval. |
| Run independent external coding workers | `flow.spawn(async: true)` | Returns a handle immediately; poll or watch, then collect output and clean up. |

Use static `fanout ... collect: all` for real DSL-level concurrency. Do not call
dynamic fanout parallel, and do not use static `collect: first` in production flows.

## Background Bash and PTY Terminal

Bash is for shell commands. The default is asynchronous:

```atman
flow run_tests() -> string {
    contract { capabilities { shell: true } }
    job = bash.spawn(cmd: "cargo test --workspace")
    status = bash.status(handle: job.handle)
    output = bash.output(handle: job.handle)
    when status.status == "running" {
        bash.kill(handle: job.handle)
    }
    return output.chunk
}
```

Use `bash.status(handle:)` to distinguish running from terminal, and
`bash.output(handle:, cursor:)` to read incremental output. Use `bash.kill(handle:)`
when the job is no longer needed. The `watch` tool is a separate LLM tool-call
mechanism for bash/terminal/agent handles; it is not a callable DSL expression
because `watch` is reserved for the DSL `watch target { ... }` declaration. Register
it immediately after spawning from the LLM tool loop, then use
`wait_for_watcher` and `watcher.unwatch` when the watcher remains active. For short
commands needing an immediate result, use `block: true` and a bounded
`block_timeout_ms`; do not put `sleep` in shell commands.

Use `term.spawn` for interactive programs requiring a PTY, such as TUI programs,
REPLs, editors, or commands whose behavior depends on terminal dimensions. Use
`term.input`, `term.capture`, `term.resize`, and `term.kill` as one lifecycle.
Use `term.find` before mouse input when text or style location matters.

The `watch` tool is separate from the DSL `watch reply { ... }` declaration. It
monitors bash, terminal, or agent handles by pattern and can be paired with
`wait_for_watcher`. Register watchers immediately after spawning, check status
before waiting, and unregister or kill resources on every terminal path.

## DSL Watch Declarations

DSL watches attach only to a variable bound directly to `llm.call`:

```atman
reply = llm.call(model: "smart", prompt: "...")
watch reply {
    on token(match: "ERROR" | "FATAL") { abort("unsafe output") }
    on elapsed(> 30 s) { warn("slow response") }
    on tokens_consumed(>= 8000) { abort("budget exceeded") }
}
```

They do not instrument `llm.extract`, arbitrary tools, subflows, fanout results, or
later aliases. Although `<` and `<=` parse, runtime enforcement currently supports
`>` and `>=`; use those forms.

## Contracts

Contracts must be the first item in a flow body:

```atman
flow run() -> string {
    contract {
        capabilities { shell: true }
        scope { read: [project_root], write: [project_root], network: [any] }
        interjection { accept: [L1, L4] }
    }
    return "ready"
}
```

`capabilities { shell: true }` is enforced for Tier Four tools such as bash and
terminal. `scope` and `interjection` are declarative metadata; filesystem access
still follows the session trust/access configuration. If a parent path can reach
shell tools through a subflow, declare the capability on the parent.

## Complete Explicit Orchestration Example

This pattern makes rule selection visible, runs independent research workers in
parallel, starts a long test in the background, and keeps the main LLM in control:

`invocation.user_message` seeds an isolated `flow.spawn` context from the named string parameter. Root turns are already recorded by the invoking client.

```atman
flow orchestrate(user_prompt: string) -> string {
    contract {
        capabilities { shell: true }
        invocation { user_message: user_prompt }
    }

    rules = rule.fetch()
    confessions = memory.fetch_confessions()
    selection = llm.extract(
        model: "cheap",
        prompt: "Choose relevant exact rule names and confession trigger hints.\n"
            + "Request: " + user_prompt + "\nRules: " + to_json_string(rules)
            + "\nConfessions: " + to_json_string(confessions),
        fields: {
            rule_names: [string] -- "exact names from the rule index",
            confession_triggers: [string] -- "short trigger search hints",
        },
    )
    { rule_names, confession_triggers } = selection
    recorded_rules = list.map(
        rule_names,
        |name| context.record(
            key: "agent.rule." + name,
            content: rule.fetch(name: name),
        ),
    )
    matched_confessions = list.reduce(
        list.map(confession_triggers, |hint| memory.fetch_confessions(trigger: hint)),
        |acc, items| concat(acc, items),
        [],
    )
    recorded_confessions = list.map(
        matched_confessions,
        |item| context.record(
            key: "agent.mistake." + item.id,
            content: to_json_string(item),
        ),
    )

    research = fanout [
        subflow(research_worker, user_prompt, "runtime"),
        subflow(research_worker, user_prompt, "tests"),
    ] collect: all
    test_result = bash.spawn(
        cmd: "cargo test --workspace",
        block: true,
        block_timeout_ms: 600000,
    )

    reply = llm.call(
        model: "smart",
        context: "session",
        system: @"prompts/system.md"
            + "\n\n## Parallel Research\n" + to_json_string(research)
            + "\n\n## Test Output\n" + test_result.output,
        tools: ["fs.read", "fs.grep"],
        stall_timeout: 600,
    )
    return text_concat(reply)
}

flow research_worker(prompt: string, area: string) -> string {
    return text_concat(llm.call(
        model: "smart",
        prompt: "Research " + area + " for: " + prompt,
        tools: ["fs.read", "fs.grep"],
    ))
}
```

The example keeps rule selection, parallel research, terminal-state observation,
output collection, and cleanup visible in the DSL. Watcher registration remains an
LLM tool-loop operation described above.
