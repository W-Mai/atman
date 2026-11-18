# @atman/client

Private browser transport for authenticated atman HTTP connections. Runtime dependencies are limited to standard browser APIs.

`HttpClient` takes protocol/schema revision requirements and typed method descriptors. A descriptor supplies the exported method name/revision, optional schema identities, result decoder and source validation.

Request parameters and descriptor identity are captured before asynchronous capability negotiation. Bearer credentials appear only in the Authorization header; endpoint credentials, query strings and fragments are rejected. Redirects are disabled. Command transport, timeout and malformed-response failures carry `ClientError.outcome === "unknown"`; they do not establish whether the command was accepted. Business request IDs remain caller-owned, and commands are never retried automatically. `RemoteError` preserves the original server error without inferring that the operation had no effects.

`subscribeFlowEvents` consumes complete durable `flow_update` batches from fetch-based SSE. UTF-8 and CR/LF framing survive read boundaries, multiline data fields are joined, and EOF discards an unterminated event. The retained cursor advances only when a validated batch is delivered. Foreign journals and conflicting receipt digests fail without changing that cursor. Closing an observer releases its response and sends no execution-control command.

Reconnect explicitly using the last delivered cursor for the same Flow. Construct a new client to renegotiate a restarted host. Response and pending-event byte budgets are bounded at 16 MiB; a client can select a smaller budget.

`bun run generate <daemon-export.json> src/generated.ts` compiles a daemon export containing `openapi`, `capabilities` and authoritative `operations` (`query` or `command` by method name). Generated DTOs, request validators, response decoders and `defineMethod` descriptors share the exported schema rules. Only advertised methods with complete supported request/response schemas and an explicit operation mode are emitted; omitted contracts and unsupported schema semantics appear in `skippedMethods` and the generation report. Method modes are supplied by the host exporter rather than inferred from names.

Schema decoding preserves required, optional, nullable, union, closed-object and guarded recursive shapes. Declared opaque JSON remains `JsonValue`. Integer decoding rejects inexact JavaScript values; independent depth and preparation budgets bound recursive validation. Unknown keywords, formats, external references and unguarded recursion are refused during generation. Schema-valued additional properties combined with named properties require an explicit supported DTO shape. Generated files reside beside `client.ts` and `schema.ts`.

```sh
bun install --frozen-lockfile
bun run quality
```
