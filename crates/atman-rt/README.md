# atman-rt

`atman-rt` is a `no_std` Rust library for executing Atman flow ASTs inside another program. It owns expression and statement dispatch, portable values and environments, list operations, fanout scheduling, cancellation precedence, and flow lifecycle ordering. It does not start an async runtime or depend on Atman tools, providers, sessions, storage, the CLI, or the daemon.

An embedding application constructs an `atman_rt::ast::FlowDecl` and runs it with `atman_rt::Engine::run_flow`. The returned future is polled by the application's executor. `StatementHost` supplies bindings, product preflight decisions, and node event publication. `ExpressionHost` supplies external effects such as tools and file reads; portable expressions and list/fanout nodes execute in the core. Both traits use the embedding application's payload and error types through `Value<P, E>`.

The [standalone embedding fixture](https://github.com/W-Mai/atman/tree/main/fixtures/atman-rt-embed) implements both host traits and runs pure flows, host effects, loops, list operations, and static and dynamic fanout without any other Atman crate dependency:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked
```

The same fixture includes an interactive `.at` demo. The `dsl-demo` feature adds `atman-dsl` only for this mode; the default embedding check still depends on `atman-rt` alone. The demo parses [`demo.at`](../../fixtures/atman-rt-embed/src/demo.at), prompts for an integer, invokes the host-provided `foreign()` effect, and prints the result. Pass a number after `--demo` to run it without a prompt:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked --features dsl-demo -- --demo
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked --features dsl-demo -- --demo 6
```

The second command prints `result: 17`.

`atman-dsl` can parse `.at` source for hosts that want a text frontend; the core accepts the AST directly and does not require the parser.

The core requires an allocator and target support for pointer-width atomics because environments and lambda captures use `Arc`. The host must poll returned futures. `wasm32-unknown-unknown` passes the `no_std` compile check, and the fixture runs on `wasm32-wasip1` with a WASI host. Targets without pointer-width atomics, including `riscv32imc-unknown-none-elf`, are not currently supported.
