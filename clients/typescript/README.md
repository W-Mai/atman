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

`FlowClient.preview()` observes an attached Flow's output tail, active turn and latest committed outcome. Pass the returned `snapshot.edition` as `known` to wait for a change. `unchanged` echoes that edition. `unavailable` indicates that a preview cannot be observed from the current owner. The query uses the existing Flow address and never opens a Session or schedules work.

`AtmanClient.readProjectDirectory()` lists project groups or pages through a single project's Sessions. Group rows include up to five Session previews. Session queries support name, newest and oldest order and case-insensitive search. A continuation belongs to its original target, sort and search. On `changed`, request a new first page. For Pin actions, use `catalog_revision` and the registered group's fingerprint.

`FlowClient.output` returns the existing final-answer publication page with opaque message bodies. `observeEvents` decodes each actual SSE body with the generated `FlowEventBatch` decoder and retains the original observer's delivery cursor. Closing the observer releases its read connection; execution controls use explicit `control` requests. `decodeDto` exposes the same checked generated DTO graph for source-bound adapters.

`FlowClient.questionModelCheck` checks a proposed model and optional reasoning setting against the original temporary Quick Ask owner and the exact observed transcript position. On success it returns the resolved model key. Clients can use that key for the next question submission.

`FlowClient.questionModelOptions` reads model and reasoning choices for a question draft or an accepted temporary question. The optional catalog search uses that target's model configuration.

`FlowClient.forkSourceReview` resolves the displayed conversation against its committed head and receipt. Supply a User or completion cutoff for an explicit boundary, or omit it to select the latest successful turn within the observation. The review returns the source catalog and project, recorded working directory, excluded turn count, serialized snapshot size, and the byte cost of copying external assets.

`FlowClient.forkReviewAt(item, head, boundary)` prepares the review request from a User item read by that client at the displayed head. Choose `"before"` for the original admission boundary or `"after"` for its successful completion within that observation, when available.

`FlowClient.createFork` submits that observation, its resolved source, the reviewed catalog and asset byte budget, and the selected destination location. The authenticated operation ID binds one destination. `forkStatus` recovers the destination after disconnection. `preparing` indicates live work, `retry_required` allows resubmission of the original confirmation, and `ready` includes the destination activation receipt and completed publication digest.

`FlowClient.approvals.observe()` reads the current User queue for an explicit Flow. For a local owner that advertises typed receipts, pass the current review snapshot and a request or group selection to `approvals.resolve()`. The returned receipt identifies the original broker decision. Check execution status for the tool result.

`FlowClient.overview()` reads the focused Flow’s Goal, Plan, Todos and recorded workspace metadata at one committed journal cursor. It can read a saved Flow without starting an execution owner. The workspace allocation describes a recorded assignment; it does not confirm that a worktree is available.

`FlowClient.conversation` returns a typed recent page or a read-capacity result for the selected Flow. For earlier messages, pass the returned `before` cursor, which binds the page to its original history head and receipt. `FlowClient.composer` reads the model, reasoning and trust presentation of the loaded Flow. When `origins` is present, each setting identifies its Session or global source. Pass `{ catalog_query: query }` to search the target model catalog by key, model or provider; an empty query lists the first matches. Omit `catalog_query` to read the current settings without the catalog. A missing catalog is distinct from an empty result. Hosts without `origins` still support the basic query.

Committed tool result parts in conversation pages can carry a plain-text `preview` of up to 2,048 UTF-8 bytes. `preview_truncated` indicates that the preview ends before the committed text. The result retains its original failure status and source ordinal when requested through `include_source_parts`. Both preview fields are optional.

On hosts that advertise `flow.conversation_artifact`, a registered result can also carry `artifact: { id, bytes }`. Pass the page's `source` and `head`, the row's `id`, its original part ordinal from `source_parts`, and the registered artifact ID in the locator to `FlowClient.conversationArtifact()`. Start at `offset: 0` with a byte `limit` between 4 and 32,768. A ready page echoes the locator and returns `next_offset` for continuation. Start a fresh observation when the source changes. A verification-budget result reports the required bytes and the host's limit.

When a verification-budget result includes `progressive: true`, retry with `verification: { id }`, using a new UUID. Repeat with the same UUID and locator while the result is `verifying`. The response reports bytes scanned, and the first `ready` response confirms the complete registration digest. Reuse the UUID to read subsequent pages. Send `verification: { id, cancel: true }` with the same locator when the reader closes. The host expires idle snapshots and caps each snapshot at 64 MiB.

On hosts that advertise `flow.conversation_diff`, request `include_work_diffs: true` on conversation pages to discover committed patches and their Work leases. Pass the page's `source` and `head` and an entry's `receipt` and `record` to `FlowClient.conversationDiff()`. Start at line offset `0` with a limit of 1–64 lines. A ready page echoes the locator, carries both hunk line numbers, and provides `next_offset` for continuation. Each line reports whether its text was clipped. The index reports when older patches were omitted.

On hosts that advertise `flow.conversation_log`, request `include_work_logs: true` to discover Bash log checkpoints. Pass the page's `source` and `head`, the checkpoint's `receipt` and `record`, and a new `verification: { id }` to `FlowClient.conversationLog()`. Start at offset 0 with a limit of 4–32,768 bytes. Retry a `verifying` response with the same identity and use `next_offset` to continue. The offset counts original bytes, including invalid UTF-8 displayed as replacement characters. A checkpoint remains fixed as its process appends output. Select the newer checkpoint explicitly to see more output. Cancel the verification with the same locator and `verification: { id, cancel: true }` when the reader closes. Snapshots are capped at 64 MiB and expire when idle.

Older Bash tool results can supply a log locator without `bytes` or `drained`. The host checks the original tool call and snapshots the file on the first read. Start an expired or cancelled observation at offset 0 with a fresh verification ID.

`FlowClient.conversationUsage()` reads one invocation at a committed conversation cursor. Pass the page's `source` and `head`, a visible row's `id`, and its `work.lease`. The result groups token counts by provider, model, call purpose, scope, and measurement source. `input` includes the recorded cache read and write lanes. `cache_read_calls` counts calls with a positive cache read, so clients can identify incomplete cache coverage. Edit totals use applied edit receipts.

`FlowClient.conversationImage()` reads one image from a stored conversation. Pass its page's `source` and `head`, original item ID and part ordinal from `source_parts` in the locator. A ready response echoes the locator and carries a validated media type and up to 4 MiB of original image bytes encoded as base64.

`FlowClient.questionImage()` takes the original Quick Ask identity and owner generations, the transcript batch's receipt, and the item and part ordinal. A kept question remains readable under this identity until its saved Flow is installed. Use `conversationImage()` with the saved Flow's conversation locator after installation.

`FlowClient.inbox` reads a bounded page of operator-authored pending inputs for the selected Flow, including its observed head and each input’s revision. `FlowClient.editInbox` applies an exact cancel, text edit, delivery change or next-turn reorder to that source. A lost mutation response can be reconciled by retrying the same request ID and parameters. The worker rechecks the observed head, row revision and required lane order before making a new edit.

`FlowClient.questionStatus` reads a Quick Ask attempt and its Session owner identity. Use that identity in `FlowClient.startQuestion` with the displayed history head, source receipt, and independent question and request IDs. A retry on the same owner uses the original captured context and acceptance. A retired owner ends the lifetime of its temporary question.

`FlowClient.questionTranscript` reads committed display rows from that temporary question. Start with `after: null` and continue from the last complete page installed by the caller. Set `wait: true` at the tail to wait briefly for an update. The temporary cursor carries both the runtime host generation and the original question ID.

```ts
import { AtmanClient, contract, HttpClient } from "@atman/client";

const api = new AtmanClient(new HttpClient({ endpoint, token, contract }));
const target = api.flow({ session_id, instance_id });
const request = { request_id: crypto.randomUUID(), user: { text: "Inspect the change" } };
const reply = await target.submit(request);
const status = await target.inputStatus(request.request_id);
```

Retain the original request after an unknown command outcome and query that same identity. Explicit retries use the same request ID and parameters. Source wrappers let a caller install late results on their original target without changing another view's draft or focus.

On hosts that advertise `project.workspaceRoots` and `project.workspaceFolders`, use `api.listWorkspaceRoots()` to discover configured roots and `api.listWorkspaceFolders({ root_id, components })` to read one directory level. Pass the returned `after` with the same root and components to continue. If the host returns `directory_changed`, begin that directory's listing again. The host returns relative names and scopes each continuation to the exact directory and authorization it observed.

Configure roots in `workspace-roots.toml` in the host's Atman configuration directory. For example, set `label = "Projects"` and `path = "/home/alex/work"` in `[roots.work]`. The list starts empty. Use absolute paths through real directories on macOS or Linux with kernel mount-identity support. Files, hidden directories, and symbolic links are excluded. A root cannot contain or sit inside Atman's configuration or data directories.

Set `allow_open = true` on a root to allow the authenticated operator to pin a selected workspace and start a Session there. Enable opening for roots whose contents, path, mounts, permissions, and Atman configuration are controlled by trusted users. Pass a folder page's `selection` and a new `operation_id` to `api.openWorkspace()`. If the connection drops, use `api.workspaceOpenStatus(operationId)` to locate the latest durable stage. Retry an unfinished operation with the same selection and ID. The result identifies the registered workspace and its Session, then returns the canonical Flow once its conversation is ready.

```sh
bun install --frozen-lockfile
bun run quality
```

`bun run build:web` packages the Svelte workbench with Vite and embeds the static HTML, JS, CSS, and shared atman logo in the daemon. `bun run build:web --check` verifies those files against their sources. `bun run check:web` type-checks the workbench with `svelte-check` and the Web workspace's TypeScript version.

For local development, start an atman daemon and run `ATMAN_WEB_DEV_ORIGIN=http://127.0.0.1:65099 bun run dev:web` from this directory. The development server forwards RPC and SSE to that daemon. Verify release behavior through the page served directly by the daemon.
