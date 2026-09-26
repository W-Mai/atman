# How to filter and map lists in atman

The `.at` DSL supports lambda expressions and static and dynamic fanout. Use the list combinators when a list operation needs flow-defined logic, and use static fanout for concurrent execution of explicit branches.

## Lambda expressions

A lambda is written as `|parameter| expression`:

```atman
flow double_values(xs: list) -> list {
    return list.map(xs, |x| x * 2)
}
```

The built-in list combinators are `list.map`, `list.filter`, `list.find`, `list.any`, `list.all`, and `list.reduce`. They evaluate lambda calls sequentially.

For example, filter non-empty strings:

```atman
flow keep_non_empty(items: list) -> list {
    return list.filter(items, |item| item != "")
}
```

## Dynamic fanout

Dynamic fanout evaluates a source expression, applies a lambda to each item, and collects the branch results:

```atman
flow summarize_items(items: list) -> list {
    return fanout items { |item| llm.call(
        model: "smart",
        prompt: "Summarize: " + item
    ) } collect: all
}
```

Dynamic fanout applies the lambda in source order. `collect: all` returns every result; `collect: first` returns the first source item's result without evaluating later items.

Static fanout evaluates an explicit list of expressions:

```atman
flow compare_files() -> list {
    return fanout [
        fs.read(path: "src/main.rs"),
        fs.read(path: "src/lib.rs")
    ] collect: all
}
```

Static fanout polls all branches concurrently and returns results in source order. `collect: first` is not yet supported for static fanout.

Use `fanout` for independent work. Keep dependent operations as ordinary sequential expressions so their data flow remains explicit.

## Loop control

Use `loop` for repeated work. `break` exits the loop and `continue` skips to the next iteration:

```atman
flow retry_until_ready() -> string {
    attempts = 0
    loop {
        attempts = attempts + 1
        when attempts >= 3 {
            break
        }
        when attempts < 2 {
            continue
        }
    }
    return "ready"
}
```

`when` bodies are ordinary statement blocks and can contain nested flow operations.

## Pipe expressions

`|>` passes the left value as the first positional argument of the call on the right:

```atman
flow first_item(xs: list) -> value {
    return xs |> first()
}
```

Use named arguments when a tool has several parameters. The pipe form is useful when each step consumes the result of the previous step.

## Current references

- Lambda parser and AST: `crates/atman-dsl/src/parse.rs`, `crates/atman-rt/src/ast.rs`
- `crates/atman-rt/src/expr.rs` and `crates/atman-runtime/src/eval/mod.rs`
- Canonical examples: `examples/agent.at`, `examples/review_code.at`, and `examples/look_into.at`
- Flow tests: `atman flow test <path>`
