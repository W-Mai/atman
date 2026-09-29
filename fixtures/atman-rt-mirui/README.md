# atman-rt mirui headless fixture

This standalone workspace connects a generated `atman-rt` stateful tool binding to `mirui` 0.46.3. The dependency enables only mirui's `std` feature; window, SDL, and WGPU backends are absent.

`mirui::App::headless`, its ECS `Entity`, and the software framebuffer remain on the main thread. The VM runs on a worker thread and retains only a `Send` command proxy plus an opaque surface lease ID.

Reply-bearing commands use a std-only wakeable oneshot `Future`. Polling registers the VM worker's waker, an owner reply unparks that worker, and the executor parks again while no future can make progress. Resource replies carry an armed RAII lease rather than a bare entity ID, so an unread or cancelled reply still schedules owner-thread release. The async tools do not block inside their futures.

```text
VM worker: ui.load/frame/draw/release -> command channel -> main thread: App::headless
```

Run from the repository root:

```sh
env CARGO_TARGET_DIR=target cargo run --manifest-path fixtures/atman-rt-mirui/Cargo.toml --locked
env CARGO_TARGET_DIR=target cargo test --manifest-path fixtures/atman-rt-mirui/Cargo.toml --locked
```

The executable verifies:

- `ui.load` returns a typed VM resource lease without moving any mirui object to the worker.
- Three VM-driven frames mutate the main-thread root style and render through `App::headless`.
- The software framebuffer hash changes on every frame.
- `ui.release` removes the owner-side lease exactly once.
- Every command originates on the VM worker and is handled on the mirui owner thread.
