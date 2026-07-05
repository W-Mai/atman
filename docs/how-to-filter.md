# How to filter and map lists in atman

`atman-runtime`'s stdlib registers six list combinators at Tier Zero:

| tool            | signature                                        |
|-----------------|--------------------------------------------------|
| `list_map`      | `(list, fn_name) -> list`                        |
| `list_filter`   | `(list, fn_name) -> list`                        |
| `list_find`     | `(list, fn_name) -> item \| unit`                |
| `list_any`      | `(list, fn_name) -> bool`                        |
| `list_all`      | `(list, fn_name) -> bool`                        |
| `list_reduce`   | `(list, fn_name, init) -> value`                 |

Each looks `fn_name` up in `ctx.registry` (the `ToolRegistry` handed to the running flow) and calls that tool once per element. Predicates (`filter` / `find` / `any` / `all`) must return `bool`; anything else surfaces `RuntimeError::TypeMismatch`.

## Filter with a built-in predicate

`is_empty` returns `true` for empty lists and empty strings. Drop the empty strings out of a list:

```atman
flow keep_non_empty(items: list) -> list {
    empties = list_filter(items, "is_empty")
    return empties
}
```

That gives you the *empty* ones back. For the negation, pair `list_map` with a Rust-side predicate that returns the inverse, or write a small tool of your own (below).

## Reduce for aggregation

`list_reduce` folds left-to-right using `fn(acc, elem) -> acc'`. Adding integers:

```atman
flow sum_of(xs: list) -> int {
    total = list_reduce(xs, "add_ints", 0)
    return total
}
```

`add_ints` is not in stdlib today — register your own (below).

## Bringing your own predicate

The combinators do **not** yet resolve `fn_name` against flows declared in the same `.at` file. Predicates must live in the tool registry, which means Rust code. Two options.

### Option 1 — write a `Tool` impl and register it

```rust
use atman_runtime::error::RuntimeError;
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use atman_runtime::value::Value;

pub struct IsBig;

impl Tool for IsBig {
    fn name(&self) -> &str {
        "is_big"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            match args.positional(0)? {
                Value::Int(n) => Ok(Value::Bool(*n > 10)),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "int".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}
```

Register once before running the flow (e.g. in the caller that owns the `Executor`):

```rust
use std::sync::Arc;

atman_runtime::tools::register_tier_zero(&mut executor.tools);
executor.tools.register(Arc::new(IsBig));
```

Now the DSL can call it:

```atman
flow big_ints(xs: list) -> list {
    return list_filter(xs, "is_big")
}
```

### Option 2 — wait for flow-callable combinators

Making `list_filter([1,2,3], "my_flow")` resolve `"my_flow"` against the current file's `flow` declarations needs an invoker plumbed through `ToolCtx`. Design lives in `.local/specs/expressiveness-tier-decision/` (Slice B); it lights up when a real workflow surfaces the request.

## Combining with the pipe operator

`|>` prepends the left value as the first positional arg of the right call. Pairing it with combinators keeps the reading direction natural:

```atman
flow show_big(xs: list) -> int {
    return xs |> list_filter("is_big") |> len()
}
```

## Reference

- Combinator source: `crates/atman-runtime/src/tools/stdlib.rs`
- Decision context: `.local/specs/expressiveness-tier-decision/`
- Sample custom `Tool` impls: search for `impl Tool for` in `crates/atman-runtime/src/tools/`
