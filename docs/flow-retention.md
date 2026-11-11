# Temporary Flow retention

Temporary Flow history uses the native state machine, publication validation and readers with an in-memory journal. `FlowPosition::Transient` identifies its host generation, Flow, sequence and content digest. It cannot certify durable storage or be written unchanged into a persistent journal. Ordinary durable positions retain their existing wire format.

`SessionStore::create_transient_flow` creates no Flow directory or catalog entry. Registered outputs share the history's bounded retention account. Allocation ownership stays with snapshots and readers until their final consumer releases it. The host limits retained payload to 128 MiB in total; each Flow allows 32 MiB, including a 256 KiB terminal reserve. Capacity failures retain readable history and permit bounded failure and closure records.

`FlowStore::capture_retention` freezes an idle history at an exact observed position after its host has fenced scheduling and drained producers. Already acknowledged pending input remains in the capture. The captured project belongs to the Flow, independently of later changes to its Session's project.

`SessionStore::retain_flow` transfers that capture through the ordinary durable journal. Batch order, Flow and turn identities, publication identities, messages and pending input remain intact. Only typed publication references are mapped to their actual durable receipts; authored text and arbitrary JSON are unchanged. Captured images and registered outputs retain their content and identities. The transfer executes no entry or tool.

Each retention request fixes its source position, destination and output digest. The storage owner writes a complete staged history, verifies it by replay, publishes its manifest and directory, then records the Flow and its project in the Session catalog. The catalog includes the acknowledged source-to-destination correspondence. It does not expose an incomplete transfer as a normal Flow.

Retries use the same request and capture. A complete destination whose catalog registration was interrupted is validated and registered without rewriting its history. An uncertain catalog write is replayed and synchronized under the original owner lock; its previously acknowledged prefix must remain unchanged. Conflicting source identities or output contents fail without replacing stored data. The memory capture remains owned by the caller until the host confirms its handoff.

`retention_receipt` reconciles the fixed request without opening another Flow writer. Hosts use it to reuse an already installed durable worker. Storage transfer, canonical worker replacement and client attachment are separate acknowledgements; creating a durable directory alone does not complete the handoff.
