# Daemon client platform

Atman uses one local daemon as the exclusive owner of persistent sessions, runs, interactions, resources, provider configuration, and MCP configuration. Terminal, command-line, monitor, browser, and future GUI clients attach through a typed SDK instead of opening session storage or reconstructing raw events independently.

## Ownership boundary

| State | Owner | Client access |
|---|---|---|
| Session messages, context heads, checkpoints, usage, and run results | Daemon session actor | Snapshot, ordered projection events, and typed commands |
| Approvals, forms, compaction reviews, and prompts | Daemon interaction services | Projected pending state and compare-and-set resolution commands |
| Processes, terminals, workspaces, and retained resources | Daemon resource supervisor | Resource projection and typed lifecycle commands |
| Provider, model, and MCP configuration | Daemon configuration services | Typed query, mutation, probe, and reload commands |
| Input draft, attached-but-unsent images, scroll, disclosure, and theme | Individual client | Local UI state only |

An attached client never opens a second writer for the same persistent session. The daemon keeps one `SessionActor` and one session-scoped runtime host for each loaded session; concurrent runs receive separate invocation environments while reusing the host's executor, provider lifecycle, MCP supervisor, memory stores, and tool registries. Explicit `atman run --mock`, `atman run --ephemeral`, `atman tui-preview`, test fixtures, and offline maintenance commands remain embedded because they do not attach to a daemon-owned persistent session.

## Process and session lifecycle

Normal `atman` TUI and `atman run` commands connect to the existing local daemon or start one detached daemon process when no live pid is present. Closing a client does not terminate the daemon or another client's run. The daemon remains available until `atman daemon stop`, SIGTERM, or Ctrl+C reaches the daemon process.

One daemon process hosts multiple session actors in-process; it does not create one Atman subprocess per session. A loaded actor owns the canonical Session, projection, interactions, active runs, and runtime host. External Bash, terminal, and MCP processes remain supervised resources rather than replacement session owners. An actor with no client lease, active run, pending interaction, blocking resource, or background compaction becomes eligible for unload after five minutes; unload flushes durable state and releases its runtime host while the daemon continues serving other sessions.

## Transports and authentication

Local terminal clients use the owner-only Unix socket at the Atman data directory's `run/atman.sock`. Browser and remote-capable clients use JSON-RPC at `http://127.0.0.1:65099/rpc` and projection events over SSE; `ATMAN_DAEMON_PORT` changes the HTTP port.

The HTTP API requires the bearer token stored in `~/.config/atman/daemon.toml`, or the path selected by `ATMAN_DAEMON_CONFIG_PATH`. Keep this token outside source code and browser URLs. `atman daemon rotate-token` replaces it while the daemon is stopped. The SDK sends the bearer in the `Authorization` header, never in an event-stream URL. Clients that require URL-only stream authentication can exchange the bearer at `/event-ticket` for a short-lived, session-scoped ticket.

The daemon currently represents one local operator. Authenticated HTTP clients and owner-only Unix clients share that principal, while each loaded session still checks its owner before any query or mutation.

## Synchronization model

Full-state attachment starts with one complete `SessionSnapshot` and its durable cursor. The TUI uses windowed attachment instead: the initial response combines current non-transcript projection state with a bounded timeline tail. Both modes then read ordered projection envelopes after the response cursor and apply each revision atomically. Duplicate events are ignored, a cursor or revision gap triggers a fresh bounded page or snapshot, and a daemon generation change reconnects through the same authoritative boundary.

Durable state converges across every transport pairing:

| Clients attached to one session | Durable messages, tools, runs, resources, forms, and approvals | Local draft, scroll, and disclosure |
|---|---|---|
| TUI + TUI | Shared and cursor-ordered | Independent |
| TUI + Web UI | Shared and cursor-ordered | Independent |
| Web UI + Web UI | Shared and cursor-ordered | Independent |

Approval resolution is compare-and-set by request revision. Concurrent decisions produce one committed result; another client receives a stale result and removes the already-resolved request when it consumes the committed projection event. Disconnecting a client does not cancel a run, and reconnecting reconstructs the same durable projection before live signals continue.

## Windowed session history

The TUI initially requests the latest 12 complete turn or session segments with a 256 KiB response budget. Scrolling near the top requests an older keyset page, search jumps request a page centered on the matched sequence, and live events patch the loaded segment by stable identity and revision. The client retains up to 48 nearby segments, preserves a per-session visual bookmark across switching, and follows the tail only while the viewport remains at the bottom.

Large tool results, terminal output, and diffs are represented by bounded previews and fetched through `session.history.item_detail` only when the row is expanded or opened fullscreen. Pages never split a visual turn merely to satisfy the byte budget; an oversized segment retains structured items with deferred detail. A validated event index locates historical byte ranges without replaying the file from its beginning. Missing or stale index coverage falls back to a bounded contiguous tail and schedules actor-backed recovery, after which the authoritative page replaces the provisional one without moving the requested anchor.

Timeline navigation is a presentation read model. `session.history.tail`, `before`, `after`, `around`, `search`, and `item_detail` do not append events, select another context head, change checkpoint epochs, run compaction, or alter provider request construction. The model context remains governed by the Session's context journal and checkpoint mechanism independently of which history pages a client has loaded.

## Rust SDK

The Rust SDK exposes `UnixTransport` and `HttpTransport`, capability negotiation, typed methods, `SessionClient`, projection watches, ephemeral signal subscriptions, and reconnecting synchronization. See [`crates/atman-client/examples/attach.rs`](../crates/atman-client/examples/attach.rs) for attach, subscribe, synchronize, and send behavior.

```bash
cargo run -p atman-client --example attach -- "$HOME/.local/share/atman/run/atman.sock"
```

The data directory can differ by platform or `ATMAN_DATA_DIR`; pass the socket path printed by `atman daemon start`.

## TypeScript SDK

`@atman/client` exposes `FetchTransport`, `AtmanClient`, generated method and projection types, `SessionClient`, and a framework-neutral subscription store. See [`packages/client/examples/attach.ts`](../packages/client/examples/attach.ts).

```bash
ATMAN_DAEMON_TOKEN='<token>' bun run packages/client/examples/attach.ts
```

The browser application supplies the token through its trusted host integration rather than embedding it in a checked-in bundle. Application code calls `FetchTransport`; it does not fetch `/rpc` directly or parse raw SSE frames.

## Compatibility behavior

Both SDKs negotiate protocol, snapshot schema, event schema, and method revisions during connection. A mismatch produces an actionable compatibility error that asks the operator to restart the daemon from the same Atman installation. Unpublished wire-shape changes update generated artifacts directly without artificial version bumps or migration branches; published incompatible contracts require an intentional protocol decision.

## Client implementation checklist

- Construct one SDK transport and complete capability negotiation before rendering session state.
- Attach through `SessionClient`; do not read `events.jsonl`, session SQLite files, or daemon storage directly.
- Use the bounded history methods for interactive transcript views; reserve full snapshots for consumers that require the complete materialized transcript.
- Render the current page or snapshot and subscribe before starting background synchronization.
- Send every mutation through a typed SDK command and retain its generated business request identity for safe retries.
- Treat projection state as durable and signals as transient presentation hints.
- Keep unsent input, image drafts, scroll, expanded rows, and theme client-local.
- Preserve viewport anchors by stable timeline item identity across prepend, search jumps, session switching, and authoritative resynchronization.
- Surface compatibility errors directly and reconnect after daemon generation changes.
