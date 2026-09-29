# atman-rt + mirui SDL demo

This standalone workspace connects generated `atman-rt` stateful tool bindings to `mirui` 0.46.3. The default `sdl` feature opens a low-saturation dashboard in a native SDL2 window; disabling default features keeps the same retained UI and VM bridge on mirui's software framebuffer for display-independent checks.

The `.at` source owns the displayed title, metrics, status, pipeline records, palette phase, loop termination, conditional branch, cooperative `yield`, async render barrier, and explicit resource release. Rust owns the mirui entity tree, validates binding inputs, retains thread-affine state, and applies ordered commands on the UI thread.

The demo binds `ui.load`, `ui.configure`, `ui.status`, `ui.metric`, `ui.stage`, `ui.boundary`, and deferred `ui.present`, plus the generated `ui.release`. `demo.at` passes the surface lease through a child flow, selects structured list records by index, uses `list.len` to terminate the loop, and awaits `ui.present()` after each batch of immediate mutations.

```text
Atman VM worker: .at flow -> typed command proxy -> mirui + SDL owner thread
```

Run from the repository root. SDL builds require SDL2 to be discoverable through `pkg-config`.

```sh
env CARGO_TARGET_DIR=target cargo run --manifest-path fixtures/atman-rt-mirui/Cargo.toml --locked
env CARGO_TARGET_DIR=target cargo test --manifest-path fixtures/atman-rt-mirui/Cargo.toml --no-default-features --locked
```

The SDL window remains open after the scripted run completes. Press Esc or close the window to exit. Set `ATMAN_MIRUI_AUTOCLOSE=1` for an automated SDL smoke run.

The fixture verifies:

- `ui.load` returns a typed VM resource lease without moving a mirui object to the worker.
- Structured `.at` data controls three distinct retained UI frames through generated bindings.
- Synchronous mutations execute in command order before the deferred present barrier replies.
- The framebuffer hash changes on every presented frame.
- Explicit and cancellation-path release remove the owner-side lease exactly once.
- Every command originates on the VM worker and is handled on the mirui owner thread.
