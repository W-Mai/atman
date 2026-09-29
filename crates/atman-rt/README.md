# atman-rt

`atman-rt` is an embeddable Atman language VM. It parses and prints `.at` source, resolves `use` and `pub` flows, validates and links modules, and executes expressions, statements, cold Flow calls, list operations, fanout, routes, watch rules, cancellation, and lifecycle hooks. It does not start an async runtime or depend on Atman tools, providers, sessions, storage, the CLI, or the daemon.

Portable list operations include `list.len`, `list.is_empty`, `list.get`, `list.first`, `list.last`, `list.tail`, `list.concat`, `list.map`, `list.filter`, `list.find`, `list.any`, `list.all`, and `list.reduce`. The existing `len`, `is_empty`, `head`, `tail`, and `concat` spellings resolve to the same core implementation.

List indexing uses zero-based `items[index]` syntax. Negative and out-of-range indexes return explicit errors. Indexing returns the stored value unchanged, so a cold Flow or tool future remains cold until `.await` or `fanout` drives it.

An embedding application calls `atman_rt::Vm::compile(Source, &resolver)` to build a VM from source text. The host implements `SourceResolver` to load imported source under its own path and trust policy. `Vm::run(flow_name, args, delegates)` executes an entry flow, `Vm::run_flow` executes a resolved flow identity, and `Vm::run_lifecycle` executes matching lifecycle bodies. Inside `.at`, `name(args)` creates a cold Flow Future, `.await` executes one call, and `fanout` drives an array of calls concurrently. The host polls the VM's returned future with its own executor. The host chooses its payload and error types through `Value<P, E>`.

Flow parameter and return annotations are checked whenever a flow body is driven. `unit`, `bool`, `int`, `float`, `string`, `list`, and `struct` match their corresponding values; `[T]` checks every item, and `{ field: T }` requires each declared field while allowing additional fields. A named host type matches `HostPayload::kind_name()`. `value` and `any` accept any resolved value. PascalCase names such as `Review` remain schema markers until the language has named type declarations, so they do not provide a runtime type guarantee. Missing required parameters, incompatible defaults, and incompatible explicit or implicit returns fail before the value crosses the flow boundary.

Integer `+`, `-`, `*`, `/`, `%`, and unary `-` use checked `i64` arithmetic. Overflow returns `ValueError::integer_overflow`; division and remainder by zero keep their dedicated errors. Debug and release builds therefore use the same integer semantics.

Operation metering is explicit. `Vm::run_with_options`, `Vm::run_flow_with_options`, and `Vm::run_lifecycle_with_options` return `VmExecution<T>` with the executed operation count. One operation is one entered statement, expression AST node, or loop iteration. Root flows, child flows, and fanout branches in the same invocation share the counter; all matching bodies in one lifecycle invocation also share its counter. Time spent waiting for host futures does not consume operations. Exceeding an explicit limit maps through `ValueError::operation_limit_exceeded`, so a host can preserve its own resource-exhaustion error type. The ordinary `run` methods do not create a counter, and the crate does not set a maximum operation count, yield interval, or deadline.

Use `VmRunOptions::measure()` on representative workloads before selecting a host limit. Record normal and worst expected flows, then pass the chosen non-zero limit through `VmRunOptions::limited(max)`. Keep scheduling yields and wall-clock deadlines as separate host policies; an operation count measures deterministic language progress rather than elapsed time.

```rust
let measured = vm
    .run_with_options("demo", args.clone(), delegates.clone(), atman_rt::VmRunOptions::measure())
    .await;
println!("operations: {}", measured.operations());

let max = core::num::NonZeroUsize::new(host_selected_limit).expect("positive host limit");
let bounded = vm
    .run_with_options("demo", args, delegates, atman_rt::VmRunOptions::limited(max))
    .await;
```

`#[atman_rt::tools]` generates a `ToolRouter` from typed Rust functions. A synchronous Rust function runs when its `.at` call is evaluated; an `async fn` creates a cold tool future that runs only on `.await` or in `fanout`. Parameter names and types come from the function signature, so the host does not need to decode `ToolArgs`:

```rust
#[atman_rt::tools]
mod host_tools {
    #[tool]
    pub async fn foreign(value: i64) -> i64 { value }
}

let tools = host_tools::router::<(), atman_rt::EvalError>()?;
let delegates = atman_rt::VmDelegates::new(tools);
let outcome = vm.run("demo", vec![("input".into(), atman_rt::Value::Int(6))], delegates).await;
```

The flow calls this tool as `foreign(value: input).await`. Building the future does not run the tool or request approval. Repeated awaits on the same future reuse its result.

Tool functions may be synchronous or asynchronous and accept `i64`, `f64`, `bool`, `String`, `()`, `Option<T>`, or `Vec<T>` with one layer of wrapping. They may return those types or `Result<T, E>`; `Option<()>` and nested wrappers are rejected at compile time. Use `#[tool(name = "namespace.name")]` to set the `.at` tool name.

For dynamic registrations, `ToolRouter::register` binds cold asynchronous closures and `ToolRouter::register_sync` binds immediate synchronous closures. The router implements the effect facet of the VM delegate contract. After compiling a `vm`, a host can register a handler manually:

```rust
let mut tools = atman_rt::ToolRouter::<(), atman_rt::EvalError>::new();
tools.register("foreign", |_| async { Ok(atman_rt::Value::Int(5)) })?;
let delegates = atman_rt::VmDelegates::new(tools);
let outcome = vm.run("demo", vec![("input".into(), atman_rt::Value::Int(6))], delegates).await;
```

Handlers receive owned, evaluated `ToolArgs`. `args.int("input", 0)?`, `string`, `float`, and `bool` read a named argument first and otherwise use the given positional index. Registration rejects duplicate, empty, and evaluator-reserved names. Missing tools fail before their arguments run and are checked again at dispatch. Handler errors become `Value::Err`; non-tool effects return an unsupported-effect error. `ToolRouter` clones are snapshots, so registering on one clone does not alter another.

`Vm::run` accepts one `VmDelegate` value as the complete execution boundary. A cohesive embedding host can implement that interface directly; `VmDelegates` is a convenience adapter that composes independent effect, authorization, observer, cancellation, flow-scope, and language-control implementations into the same interface. Authorization returns a typed, one-use permit that the VM passes to the matching effect invocation. The cancellation callback supplies both checkpoint errors and a wakeable `cancelled` future, so pending authorization, effects, child flows, and root flows can stop without another VM poll source. A signalled cancellation completes the normal `exit_flow` callback with a cancelled outcome; `abort_flow` is reserved for a driving future that is dropped before an outcome exists. `VmEvent` reports root and child flows, statements, loop iterations, fanout branches, authorization, effects, and cancellation with VM-owned context. Authorization and effect phases for one real execution share a `VmEffectInvocationId`; `VmEffectInvocation` distinguishes the context that created a cold future from the context that eventually drove it. Raw arguments and results stay behind the delegate boundary, while `effect_input_preview` and `effect_result_preview` let the host publish redacted audit text. Every started scope receives an `Ok`, `Err`, or `Cancelled` terminal event even when the driving future is dropped. `VmContext` carries VM-local run and parent-run identities, source and flow identities, node ancestry, drive mode, and fanout branch identity.

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
