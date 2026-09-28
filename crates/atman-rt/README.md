# atman-rt

`atman-rt` is an embeddable Atman language VM. It parses and prints `.at` source, resolves `use` and `pub` flows, validates and links modules, and executes expressions, statements, cold Flow calls, list operations, fanout, routes, watch rules, cancellation, and lifecycle hooks. It does not start an async runtime or depend on Atman tools, providers, sessions, storage, the CLI, or the daemon.

An embedding application calls `atman_rt::Vm::compile(Source, &resolver)` to build a VM from source text. The host implements `SourceResolver` to load imported source under its own path and trust policy. `Vm::run(flow_name, args, host)` executes an entry flow; inside `.at`, `name(args)` creates a cold Flow Future, `.await` executes one call, and `fanout` drives an array of calls concurrently. The host polls the VM's returned future with its own executor. The host chooses its payload and error types through `Value<P, E>`.

`#[atman_rt::tools]` generates a `ToolRouter` from typed Rust functions. Parameter names and types come from the function signature, so the host does not need to decode `ToolArgs`:

```rust
#[atman_rt::tools]
mod host_tools {
    #[tool]
    pub async fn foreign(value: i64) -> i64 { value }
}

let tools = host_tools::router::<(), atman_rt::EvalError>()?;
let outcome = vm.run("demo", vec![("input".into(), atman_rt::Value::Int(6))], tools).await;
```

Tool functions may be synchronous or asynchronous and accept `i64`, `f64`, `bool`, `String`, `()`, `Option<T>`, or `Vec<T>` with one layer of wrapping. They may return those types or `Result<T, E>`; `Option<()>` and nested wrappers are rejected at compile time. Use `#[tool(name = "namespace.name")]` to set the `.at` tool name.

For dynamic registrations, `ToolRouter` also binds asynchronous closures by name and implements `VmEmbedding` directly. After compiling a `vm`, a host can register a handler manually:

```rust
let mut tools = atman_rt::ToolRouter::<(), atman_rt::EvalError>::new();
tools.register("foreign", |_| async { Ok(atman_rt::Value::Int(5)) })?;
let outcome = vm.run("demo", vec![("input".into(), atman_rt::Value::Int(6))], tools).await;
```

Handlers receive owned, evaluated `ToolArgs`. `args.int("input", 0)?`, `string`, `float`, and `bool` read a named argument first and otherwise use the given positional index. Registration rejects duplicate, empty, and evaluator-reserved names. Missing tools fail before their arguments run and are checked again at dispatch. Handler errors become `Value::Err`; non-tool effects return an unsupported-effect error. `ToolRouter` clones are snapshots, so registering on one clone does not alter another. Applications that need authorization, lifecycle hooks, or other effects can implement `VmEmbedding` and delegate only tool calls to `ToolRouter::dispatch`.

Default features enable the text parser and tool macros. With `default-features = false`, a host can construct a linked program from AST and execute it through `Vm::new` without those dependencies. The VM uses `no_std` and `alloc` in this configuration.

The [standalone embedding fixture](https://github.com/W-Mai/atman/tree/main/fixtures/atman-rt-embed) runs pure flows, host effects, loops, list operations, and static and dynamic fanout without any other Atman crate dependency:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked
```

The same fixture includes an interactive `.at` demo using `Vm::compile` and `ToolRouter`. The demo compiles [`demo.at`](../../fixtures/atman-rt-embed/src/demo.at) with a host source resolver, links a `pub flow` from `helper.at`, prompts for an integer, invokes the registered `foreign()` tool, and prints the result. Pass a number after `--demo` to run it without a prompt:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked -- --demo
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked -- --demo 6
```

The second command prints `result: 18`.

`atman_rt::parse_file` parses `.at` source and `atman_rt::print_file` prints an AST. `Engine`, `StatementHost`, and `ExpressionHost` remain available for lower-level integration; `Vm` handles source linking and flow calls for the normal embedding path.

The core requires an allocator and target support for pointer-width atomics because environments and lambda captures use `Arc`. The host must poll returned futures. `wasm32-unknown-unknown` passes the `no_std` compile check, and the fixture runs on `wasm32-wasip1` with a WASI host. Targets without pointer-width atomics, including `riscv32imc-unknown-none-elf`, are not currently supported.
