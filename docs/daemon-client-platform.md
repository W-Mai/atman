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

An attached client never opens a second writer for the same persistent session. Explicit `atman run --mock`, `atman run --ephemeral`, `atman tui-preview`, test fixtures, and offline maintenance commands remain embedded because they do not attach to a daemon-owned persistent session.

## Transports and authentication

Local terminal clients use the owner-only Unix socket at the Atman data directory's `run/atman.sock`. Browser and remote-capable clients use JSON-RPC at `http://127.0.0.1:65099/rpc` and projection events over SSE; `ATMAN_DAEMON_PORT` changes the HTTP port.

The HTTP API requires the bearer token stored in `~/.config/atman/daemon.toml`, or the path selected by `ATMAN_DAEMON_CONFIG_PATH`. Keep this token outside source code and browser URLs. `atman daemon rotate-token` replaces it while the daemon is stopped. Browser event streams exchange the bearer for a short-lived, session-scoped ticket so the long-lived token does not appear in URLs, history, or access logs.

The daemon currently represents one local operator. Authenticated HTTP clients and owner-only Unix clients share that principal, while each loaded session still checks its owner before any query or mutation.

## Synchronization model

Attachment starts with one complete `SessionSnapshot` and its durable cursor. The SDK then reads ordered projection envelopes after that cursor and applies each revision atomically. Duplicate events are ignored, a cursor or revision gap triggers a fresh snapshot, and a daemon generation change reconnects through the same snapshot boundary.

Durable state converges across every transport pairing:

| Clients attached to one session | Durable messages, tools, runs, resources, forms, and approvals | Local draft, scroll, and disclosure |
|---|---|---|
| TUI + TUI | Shared and cursor-ordered | Independent |
| TUI + Web UI | Shared and cursor-ordered | Independent |
| Web UI + Web UI | Shared and cursor-ordered | Independent |

Approval resolution is compare-and-set by request revision. Concurrent decisions produce one committed result; another client receives a stale result and removes the already-resolved request when it consumes the committed projection event. Disconnecting a client does not cancel a run, and reconnecting reconstructs the same durable projection before live signals continue.

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
- Render the current snapshot and subscribe before starting background synchronization.
- Send every mutation through a typed SDK command and retain its generated business request identity for safe retries.
- Treat projection state as durable and signals as transient presentation hints.
- Keep unsent input, image drafts, scroll, expanded rows, and theme client-local.
- Surface compatibility errors directly and reconnect after daemon generation changes.
