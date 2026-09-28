# atman-rt

`atman-rt` is an embeddable Atman language VM. It parses and prints `.at` source, resolves `use` and `pub` flows, validates and links modules, and executes expressions, statements, cold Flow calls, list operations, fanout, routes, watch rules, cancellation, and lifecycle hooks. It does not start an async runtime or depend on Atman tools, providers, sessions, storage, the CLI, or the daemon.

An embedding application calls `atman_rt::Vm::compile(Source, &resolver)` to build a VM from source text. The host implements `SourceResolver` to load imported source under its own path and trust policy, and `VmEmbedding` to dispatch evaluated external effects. `Vm::run(flow_name, args, host)` executes an entry flow; inside `.at`, `name(args)` creates a cold Flow Future, `.await` executes one call, and `fanout` drives an array of calls concurrently. The host polls the VM's returned future with its own executor. `VmEmbedding` also has optional callbacks for tool preflight before argument evaluation, cancellation, node observation, and child-flow context. A host that authorizes tools should recheck authorization when dispatching the effect. The host chooses its payload and error types through `Value<P, E>`.

The default `syntax` feature includes the text parser. With `default-features = false`, a host can construct a linked program from AST and execute it through `Vm::new` without the parser dependency. The VM uses `no_std` and `alloc`; `syntax` is disabled for the checked `no_std` dependency configuration.

The [standalone embedding fixture](https://github.com/W-Mai/atman/tree/main/fixtures/atman-rt-embed) runs pure flows, host effects, loops, list operations, and static and dynamic fanout without any other Atman crate dependency:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked
```

The same fixture includes an interactive `.at` demo using `Vm::compile` and a `VmEmbedding` host. The demo compiles [`demo.at`](../../fixtures/atman-rt-embed/src/demo.at) with a host source resolver, links a `pub flow` from `helper.at`, prompts for an integer, invokes the host-provided `foreign()` effect, and prints the result. Pass a number after `--demo` to run it without a prompt:

```sh
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked -- --demo
cargo run --manifest-path fixtures/atman-rt-embed/Cargo.toml --locked -- --demo 6
```

The second command prints `result: 18`.

`atman_rt::parse_file` parses `.at` source and `atman_rt::print_file` prints an AST. `Engine`, `StatementHost`, and `ExpressionHost` remain available for lower-level integration; `Vm` handles source linking and flow calls for the normal embedding path.

The core requires an allocator and target support for pointer-width atomics because environments and lambda captures use `Arc`. The host must poll returned futures. `wasm32-unknown-unknown` passes the `no_std` compile check, and the fixture runs on `wasm32-wasip1` with a WASI host. Targets without pointer-width atomics, including `riscv32imc-unknown-none-elf`, are not currently supported.
