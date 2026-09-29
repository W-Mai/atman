# atman-rt owner-thread host fixture

This standalone workspace depends only on `atman-rt`. It exercises a generated stateful tool binding through a portable fake graphics owner.

The VM runs on a worker thread and retains only a `Send` command proxy plus opaque resource leases. `FakeWorld`, texture records, frame state, drawing, and destruction stay on the thread that created them. Dropping a lease after `demo.release` sends the destruction command back to that owner thread.

Reply-bearing commands use a std-only oneshot `Future`. Polling registers the VM worker's waker, an owner reply unparks that worker, and the executor parks again while no future can make progress. The async tools do not block inside their futures.

```text
VM worker: demo.load/frame/draw/release -> command channel -> owner thread: FakeWorld
```

Run from the repository root:

```sh
env CARGO_TARGET_DIR=target cargo run --manifest-path fixtures/atman-rt-host-demo/Cargo.toml --locked
env CARGO_TARGET_DIR=target cargo test --manifest-path fixtures/atman-rt-host-demo/Cargo.toml --locked
```

The executable verifies:

- A deferred `demo.load` call has no side effect until `.await` drives it.
- `load -> frame -> draw` runs for three ordered frames through one stateful host instance.
- `TextureLease` is stored behind a typed generational resource handle.
- `demo.release` destroys the owner-thread texture exactly once.
- Reusing the released VM handle returns a stale-resource error before another draw command reaches the owner.
- Every command originates on the VM worker and is handled on the owner thread.
- A withheld owner reply remains pending until a wakeable VM cancellation interrupts the tool effect.
- A reply sent after cancellation is rejected safely because the dropped future closes its oneshot receiver.
