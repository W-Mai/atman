# Durable fact journals

`atman_runtime::event_log::journal::Journal` commits typed batches of host facts. It is separate from the asynchronous display/event writer: an accepted commit means the entire batch has been written and synchronized. A caller publishes the resulting in-memory projection and receipt only after `commit` succeeds. Streaming previews need no durable commit per chunk.

`Journal::create(path, id)` creates a new file under an existing durable parent directory. `Journal::open(path, expected_id, apply)` holds an exclusive file lock, validates the log identity and format, and replays complete batches into a private projection. The caller must discard that projection if opening fails. These operations use blocking filesystem I/O and belong on a storage worker, outside UI or executor polling.

Each JSONL journal starts with a format/version/identity header. A batch contains a consecutive sequence number, the previous record digest, the exact serialized JSON payload, and its SHA-256 digest. Payloads are nonempty arrays. The hash chain checks integrity and ordering; it does not authenticate the caller or replace host authorization. A `CommitReceipt` identifies the journal and committed sequence/digest.

The writer serializes the complete batch before touching the file. A write or synchronization error prevents further commits through that handle. Reopening can discover a complete batch whose acknowledgement was lost; the host reconciles stable request IDs before retrying effects. A journal receipt alone does not implement request deduplication or prove that a business operation completed.

Recovery preserves an unterminated final record in an adjacent `.incomplete-<uuid>` file before removing it from the active journal. `RecoveredTail` reports the retained path, original offset, and byte count. Complete malformed records, broken digest chains, sequence gaps, unsupported schemas, or projection failures reject opening without skipping history. Recovery never executes flow code.

Existing published session event files continue to use their existing reader and writer. They are not interpreted as fact journals or rewritten by this API. An embedding host must explicitly choose the new format and bind each journal to its session or flow identity.
