# @atman/client

Private browser transport for authenticated atman HTTP connections. Runtime dependencies are limited to standard browser APIs.

`HttpClient` takes protocol/schema revision requirements and typed method descriptors. A descriptor supplies the exported method name/revision, optional schema identities, result decoder and source validation.

Request parameters and descriptor identity are captured before asynchronous capability negotiation. Bearer credentials appear only in the Authorization header; endpoint credentials, query strings and fragments are rejected. Redirects are disabled. Command transport, timeout and malformed-response failures carry `ClientError.outcome === "unknown"`; they do not establish whether the command was accepted. Business request IDs remain caller-owned, and commands are never retried automatically. `RemoteError` preserves the original server error without inferring that the operation had no effects.

`subscribeFlowEvents` consumes complete durable `flow_update` batches from fetch-based SSE. UTF-8 and CR/LF framing survive read boundaries, multiline data fields are joined, and EOF discards an unterminated event. The retained cursor advances only when a validated batch is delivered. Foreign journals and conflicting receipt digests fail without changing that cursor. Closing an observer releases its response and sends no execution-control command.

Reconnect explicitly using the last delivered cursor for the same Flow. The Web workbench resumes this observation automatically. A `ClientError` from an HTTP response carries its status. Construct a new client to renegotiate a restarted host. Response and pending-event byte budgets are bounded at 16 MiB; a client can select a smaller budget.

`bun run generate <daemon-export.json> src/generated.ts` compiles a daemon export containing `openapi`, `capabilities` and authoritative `operations` (`query` or `command` by method name). Generated DTOs, request validators, response decoders and `defineMethod` descriptors share the exported schema rules. Only advertised methods with complete supported request/response schemas and an explicit operation mode are emitted; omitted contracts and unsupported schema semantics appear in `skippedMethods` and the generation report. Method modes are supplied by the host exporter rather than inferred from names.

The package exports the generated `contract`, `methods`, `skippedMethods` and `Dtos`, including human input submission and canonical conversation startup. Methods without a complete advertised schema pair remain explicitly unavailable in that catalog.

Schema decoding preserves required, optional, nullable, union, closed-object and guarded recursive shapes. Property-name constraints validate every named or additional object key through the same reference graph and preparation budget. Declared opaque JSON remains `JsonValue`. Integer decoding rejects inexact JavaScript values; independent depth and preparation budgets bound recursive validation. Unknown keywords, formats, external references and unguarded recursion are refused during generation. Schema-valued additional properties combined with named properties require an explicit supported DTO shape. Generated files reside beside `client.ts` and `schema.ts`.

`AtmanClient` creates immutable `SessionClient` and `FlowClient` targets over the same HTTP transport. Session create/open, Flow catalog pages and canonical bootstrap return the original Session identity. Human submission, input-status lookup, explicit control, final-answer pages, publication details and event pages return their original Flow binding. Reply checks retain the request UUID, publication identity, exact journal receipts, page limits and registered artifact identities. The facade keeps caller-owned submission IDs and sends authored input through `flow.submit`.

`SessionClient.flows` returns a bounded catalog page. Readable entries can include `map` with a visible parent address in the same Session, an execution phase, and a pause flag. Omission is supported for older hosts and unreadable entries. A parent may be on another page; the page catalog receipt identifies which results can be combined.

`FlowClient.output` returns the existing final-answer publication page with opaque message bodies. `observeEvents` decodes each actual SSE body with the generated `FlowEventBatch` decoder and retains the original observer's delivery cursor. Closing the observer releases its read connection; execution controls use explicit `control` requests. `decodeDto` exposes the same checked generated DTO graph for source-bound adapters.

`FlowClient.approvals.observe()` reads the current User queue for an explicit Flow. For a local owner that advertises typed receipts, pass the current review snapshot and a request or group selection to `approvals.resolve()`. The returned receipt identifies the original broker decision. Check execution status for the tool result.

`FlowClient.overview()` reads the focused Flow’s Goal, Plan, Todos and recorded workspace metadata at one committed journal cursor. It can read a saved Flow without starting an execution owner. The workspace allocation describes a recorded assignment; it does not confirm that a worktree is available.

`FlowClient.conversation` returns a typed recent page or a read-capacity result for the selected Flow. For earlier messages, pass the returned `before` cursor, which binds the page to its original history head and receipt. `FlowClient.composer` reads the model, reasoning and trust presentation of the loaded Flow. When `origins` is present, each setting identifies its Session or global source. Pass `{ catalog_query: query }` to search the target model catalog by key, model or provider; an empty query lists the first matches. Omit `catalog_query` to read the current settings without the catalog. A missing catalog is distinct from an empty result. Hosts without `origins` still support the basic query.

`FlowClient.inbox` reads a bounded page of operator-authored pending inputs for the selected Flow, including its observed head and each input’s revision. `FlowClient.editInbox` applies an exact cancel, text edit, delivery change or next-turn reorder to that source. A lost mutation response can be reconciled by retrying the same request ID and parameters. The worker rechecks the observed head, row revision and required lane order before making a new edit.

```ts
import { AtmanClient, contract, HttpClient } from "@atman/client";

const api = new AtmanClient(new HttpClient({ endpoint, token, contract }));
const target = api.flow({ session_id, instance_id });
const request = { request_id: crypto.randomUUID(), user: { text: "Inspect the change" } };
const reply = await target.submit(request);
const status = await target.inputStatus(request.request_id);
```

Retain the original request after an unknown command outcome and query that same identity. Explicit retries use the same request ID and parameters. Source wrappers let a caller install late results on their original target without changing another view's draft or focus.

```sh
bun install --frozen-lockfile
bun run quality
```

`bun run build:web` packages the Svelte workbench with Vite and embeds the static HTML, JS, CSS, and shared atman logo in the daemon. `bun run build:web --check` verifies those files against their sources. `bun run check:web` type-checks the workbench with `svelte-check` and the Web workspace's TypeScript version.

For local development, start an atman daemon and run `ATMAN_WEB_DEV_ORIGIN=http://127.0.0.1:65099 bun run dev:web` from this directory. The development server forwards RPC and SSE to that daemon. Verify release behavior through the page served directly by the daemon.
