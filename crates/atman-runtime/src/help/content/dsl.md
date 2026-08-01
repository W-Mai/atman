# Atman DSL Syntax Reference

## Top-level declarations

| Syntax | Description |
|--------|-------------|
| `flow name(params) -> ret { contract? stmts }` | Flow declaration. `-> ret` and `contract` optional. |
| `route "pattern" { flow: name }` | Route pattern → flow mapping |
| `default_route { flow: name }` | Fallback flow for unmatched input |
| `on session.start { stmts }` | Lifecycle hook: `session.start`, `session.end`, `session.context_compact`, `turn.start`, `turn.end` |

## Statements

| Syntax | Description |
|--------|-------------|
| `name = expr` | Bind expression to variable |
| `{ field: alias, field2 } = expr` | Destructure struct |
| `when cond { stmts }` | Conditional block |
| `return expr` | Return from flow |
| `expr` | Bare expression (side effect) |
| `watch target { on event { actions } }` | Watch declaration |

## Expressions

### Literals

| Syntax | Type |
|--------|------|
| `"..."` | string |
| `123` | int |
| `1.5` | float |
| `true` / `false` | bool |
| `@"/path"` | file reference |

### Operators (precedence low → high)

| Prec | Operator | Description |
|------|----------|-------------|
| 0 | `\|>` | Pipe |
| 1 | `\|\|` | Logical OR |
| 2 | `&&` | Logical AND |
| 3 | `== != < <= > >=` | Comparison |
| 4 | `+ -` | Add / Subtract |
| 5 | `* / %` | Multiply / Divide / Modulo |
| — | `!expr` | Unary NOT |
| — | `-expr` | Unary negate |

### Special nodes

#### `llm { kwargs }` — LLM call

```
llm {
    model: "smart",
    context: session,              // or messages: [...]
    system: "system prompt",
    messages: [...],
    input: { ... },
    schema: TypeName,
    cache: true,
    retry: 3,
    tools: [fs.read, "mcp.*"],
}
```

#### `fanout [items] collect: all|first` — Parallel fanout

#### `user_confirm(msg)` — User confirmation gate (returns bool)

#### `subflow(name, args...)` — Subflow invocation (enables recursion)

#### `fix_until_test_passes { kwargs }` — Edit-test loop

#### Message constructors

`user_msg(...)`, `assistant_msg(...)`, `system_msg(...)`, `tool_result(...)`

### Watch declarations

```
watch reply {
    on token(match: "ERROR" | "FATAL") { abort("...") }
    on elapsed(> 30000 ms) { warn("...") }
    on tokens_consumed(>= 8000) { warn("...") }
}
```

Actions: `abort(msg?)` or `warn(msg?)`

### Contract blocks

```
contract {
    scope { read: [project_root], write: [project_root], network: [any] }
    capabilities { shell: true }
    interjection { accept: [L1, L4] }
}
```

## Types

| Syntax | Description |
|--------|-------------|
| `string` `int` `bool` `path` `float` | Primitives |
| `TypeName` | Named type |
| `[Type]` | List type |
| `{ field: Type, ... }` | Struct type |

## Example

```
flow review_code(file: path) -> Review {
    contract {
        scope { read: [project_root], write: [] }
    }
    gather = fanout [fs.read(file), fetch_rule("code-review")] collect: all
    primary = llm {
        model: "smart",
        messages: [system_msg(@"prompts/review.md"), user_msg("Review.")]
        input: gather
        schema: Review
    }
    return primary
}
```
