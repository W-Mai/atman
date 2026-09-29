# atman-rt

`atman-rt` is an embeddable Atman language VM. It parses and prints `.at` source, resolves `use` and `pub` flows, validates and links modules, and executes expressions, statements, cold Flow calls, list operations, fanout, explicit yields, routes, watch rules, cancellation, and lifecycle hooks. It does not start an async runtime or depend on Atman tools, providers, sessions, storage, the CLI, or the daemon.

Portable list operations include `list.len`, `list.is_empty`, `list.get`, `list.first`, `list.last`, `list.tail`, `list.concat`, `list.map`, `list.filter`, `list.find`, `list.any`, `list.all`, and `list.reduce`. The existing `len`, `is_empty`, `head`, `tail`, and `concat` spellings resolve to the same core implementation.

List indexing uses zero-based `items[index]` syntax. Negative and out-of-range indexes return explicit errors. Indexing returns the stored value unchanged, so a cold Flow or tool future remains cold until `.await` or `fanout` drives it.

An embedding application calls `atman_rt::Vm::compile(Source, &resolver)` to build a VM from source text. The host implements `SourceResolver` to load imported source under its own path and trust policy. `Vm::run(flow_name, args, delegates)` executes an entry flow, `Vm::run_flow` executes a resolved flow identity, and `Vm::run_lifecycle` executes matching lifecycle bodies. Inside `.at`, `name(args)` creates a cold Flow Future, `.await` executes one call, and `fanout` drives an array of calls concurrently. The host polls the VM's returned future with its own executor. The host chooses its payload and error types through `Value<P, E>`.

Flow parameter and return annotations are checked whenever a flow body is driven. `unit`, `bool`, `int`, `float`, `string`, `list`, and `struct` match their corresponding values; `[T]` checks every item, and `{ field: T }` requires each declared field while allowing additional fields. A named host type matches `HostPayload::kind_name()`. `value` and `any` accept any resolved value. PascalCase names such as `Review` remain schema markers until the language has named type declarations, so they do not provide a runtime type guarantee. Missing required parameters, incompatible defaults, and incompatible explicit or implicit returns fail before the value crosses the flow boundary.

Integer `+`, `-`, `*`, `/`, `%`, and unary `-` use checked `i64` arithmetic. Overflow returns `ValueError::integer_overflow`; division and remainder by zero keep their dedicated errors. Debug and release builds therefore use the same integer semantics.

Operation control is explicit. `Vm::run_with_options`, `Vm::run_flow_with_options`, and `Vm::run_lifecycle_with_options` return `VmExecution<T>` with execution statistics. One operation is one entered statement, expression AST node, or loop iteration. Root flows, child flows, and fanout branches in the same invocation share the counter; all matching bodies in one lifecycle invocation also share its counter. Time spent waiting for host futures does not consume operations. Exceeding an explicit limit maps through `ValueError::operation_limit_exceeded`, so a host can preserve its own resource-exhaustion error type. The ordinary `run` methods do not create a counter, and the crate does not set a maximum operation count, yield interval, or deadline.

An `.at` flow can write `yield` as a standalone statement. It self-wakes the VM task, returns `Pending` once, and resumes with the following statement on the next poll. Source yields work through the ordinary `run` methods, consume one operation when metering is enabled, and do not require a host tool or `.await`.

`VmRunOptions::with_yield_interval` adds host-controlled cooperative scheduling to an explicitly controlled run. After an interval of completed operations, a remaining language checkpoint self-wakes the task and returns `Pending` once before it starts the next operation. A flow that finishes exactly at an interval does not yield. Operation limits remain exact and do not yield at a limit that already prevents the next operation. In concurrent fanout, the interval is a scheduling target rather than a hard per-poll operation cap: branches already waiting on an older epoch may each advance one operation so a long early branch cannot starve later branches, and the overshoot therefore depends on the number of simultaneous waiters. `cooperative_yields()` counts host-controlled invocation-wide yield epochs rather than source `yield` statements or the number of branches that returned `Pending`; it is not derived solely from the final operation count. Wall-clock deadlines remain a host cancellation or timeout policy.

Use `VmRunOptions::measure()` on representative workloads before selecting a host limit or yield interval. Record normal and worst expected flows, then pass each chosen non-zero value explicitly.

```rust
let measured = vm
    .run_with_options("demo", args.clone(), delegates.clone(), atman_rt::VmRunOptions::measure())
    .await;
println!("operations: {}", measured.operations());

let max = core::num::NonZeroUsize::new(host_selected_limit).expect("positive host limit");
let yield_interval = core::num::NonZeroUsize::new(host_selected_yield_interval)
    .expect("positive host yield interval");
let bounded = vm
    .run_with_options(
        "demo",
        args,
        delegates,
        atman_rt::VmRunOptions::limited(max).with_yield_interval(yield_interval),
    )
    .await;
println!("cooperative yields: {}", bounded.cooperative_yields());
```

`#[atman_rt::tools]` on an inline module generates a `ToolRouter` from typed Rust functions. A synchronous Rust function runs when its `.at` call is evaluated; an `async fn` creates a cold tool future that runs only on `.await` or in `fanout`. Parameter names and types come from the function signature, so the host does not need to decode `ToolArgs`:

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

Tool functions may be synchronous or asynchronous and use the built-in scalar types, `String`, `()`, `Option<T>`, `Vec<T>`, or types generated by `#[atman_rt::value]`. They may return those types or `Result<T, E>`; ambiguous optional forms such as `Option<()>` and nested `Option` are rejected at compile time. Use `#[tool(name = "namespace.name")]` to set the `.at` tool name.

`#[atman_rt::value]` generates VM codecs and the matching portable `TypeSpec` from a Rust type. Named-field structs map to `.at` structs, fieldless enums map to their variant-name strings, and fields may recursively contain supported scalars, lists, options, or other generated values. Decode errors retain the argument, field, and list-index path; extra struct fields are ignored while duplicate or missing required fields fail.

`#[atman_rt::resource]` marks an opaque nominal host resource. A stateful `#[atman_rt::tools(namespace = "...")]` impl owns one shared host instance, while every stateful binding mounted through one `ToolRouter` clone lineage shares one typed generational resource registry. Synchronous methods may borrow a live resource as `&T`. Asynchronous methods cannot accept resource-borrow parameters; they may return an owned resource.

```rust
#[atman_rt::value]
struct Point {
    x: i32,
    y: i32,
}

#[atman_rt::resource]
struct Texture(i64);

struct GraphicsHost {
    offset: i64,
}

#[atman_rt::tools(namespace = "gfx")]
impl GraphicsHost {
    #[tool]
    async fn load(&self, id: i64) -> Texture {
        Texture(id + self.offset)
    }

    #[tool]
    fn draw(&self, texture: &Texture, point: Point) -> i64 {
        texture.0 + i64::from(point.x) + i64::from(point.y)
    }
}

type Payload = atman_rt::resource::WithResources<()>;
let mut tools = atman_rt::ToolRouter::<Payload, atman_rt::EvalError>::new();
tools.mount(GraphicsHost { offset: 4 }.into_atman_binding())?;

let catalog = tools.catalog();
vm.validate_tools(&catalog)?;
let delegates = atman_rt::VmDelegates::new(tools);
```

`ToolRouter::mount` adds all handler and catalog entries from one generated binding atomically; a collision leaves the target router and its existing clones unchanged and does not publish a newly created candidate registry to the lineage. Router clones share their resource lineage even when different stateful bindings are mounted after the clone. A concurrent or reentrant initial mount in the same lineage returns `ToolRegisterError::MountInProgress`. An unmounted sibling does not keep the shared registry alive. `ToolRouter::catalog` returns the stable-order names, immediate or deferred modes, documentation, parameters, results, structured value shapes, and nominal resource types generated from the same Rust signatures. Entries created through `register` or `register_sync` remain usable and appear without a portable signature.

`Vm::validate_tools(&catalog)` is an explicit post-link check. It covers every linked module plus defaults, contracts, and lifecycle bodies, and reports missing tools, invalid argument shapes, statically determinable literal type errors, and `.await` on an immediate tool. Ordinary `Vm::compile` remains suitable for hosts whose tools arrive dynamically, and runtime codecs and resource checks remain authoritative.

Every stateful binding also generates an immediate `<namespace>.release(resource)` tool. A successful release removes any resource from the same router lineage, drops the stored value, and makes every copied handle stale; an active shared borrow returns a resource-busy error without invalidating the handle. Handle drop alone does not release a live resource. Handles from an independent router return a foreign-resource error.

Committed resources use router-lineage ownership. A later flow error or cancellation does not implicitly release them; if the handle becomes unreachable, the shared registry retains the resource until the last router or generated handler snapshot that owns that registry is dropped. Flows that need earlier destruction must call a generated release tool on every controlled exit path.

Resource-bearing outputs are encoded transactionally. Encoding reserves pending slots whose handles cannot be borrowed or released. The outer successful conversion publishes the complete batch at once; an error or unwind rolls the batch back in reverse order, invalidates every pending handle, and drops the resource values. Each generated handler uses an isolated output context, so concurrent calls cannot commit or roll back each other's resources.

```atman
texture = gfx.load(id: 7).await
result = gfx.draw(texture: texture, point: { x: 2, y: 3 })
gfx.release(texture)
```

Thread-affine UI and graphics objects can stay on their owner thread. Store a `Send + Sync` proxy lease containing an owner-side ID and command sender as the `#[resource]` value, let VM tools send ordered commands through that bridge, and let release drop the lease so destruction is scheduled on the owner thread. The real object never enters the VM worker or receives an unsafe thread-safety wrapper.

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

The [portable owner-thread fixture](https://github.com/W-Mai/atman/tree/main/fixtures/atman-rt-host-demo) verifies stateful bindings, cold async tools, three ordered frames, explicit release, stale-handle rejection, cleanup when async resource creation is cancelled before publication, and owner-thread destruction using a fake graphics host. The [mirui headless fixture](https://github.com/W-Mai/atman/tree/main/fixtures/atman-rt-mirui) keeps `mirui::App::headless`, its ECS entities, and its software framebuffer on the main thread while the VM runs on a worker. `atman-rt` is the only Atman crate dependency in both fixtures; the second fixture also depends on `mirui = 0.46.3` with default features disabled and `std` enabled:

```sh
env CARGO_TARGET_DIR=target cargo run --manifest-path fixtures/atman-rt-host-demo/Cargo.toml --locked
env CARGO_TARGET_DIR=target cargo run --manifest-path fixtures/atman-rt-mirui/Cargo.toml --locked
```

`atman_rt::parse_file` parses `.at` source and `atman_rt::print_file` prints an AST. `Engine`, `StatementHost`, and `ExpressionHost` remain available for lower-level integration; `Vm` handles source linking and flow calls for the normal embedding path.

The core requires an allocator and target support for pointer-width atomics because environments and lambda captures use `Arc`. The host must poll returned futures. `wasm32-unknown-unknown` passes the `no_std` compile check, and the fixture runs on `wasm32-wasip1` with a WASI host. Targets without pointer-width atomics, including `riscv32imc-unknown-none-elf`, are not currently supported.
