# Permission control

Atman uses a layered permission system to decide whether a tool call runs automatically, waits for approval, or is denied. The decision combines the current Session's trust configuration, the tool's Tier, structured resource risks, and the authority ceiling established when the Flow started.

## The decision model

A tool call passes through the central permission gate before it performs a side effect:

```text
Tool invocation
  → structured resource provenance
  → Tier and risk policy
  → Flow authority ceiling
  → PermissionBroker
  → auto-run, approval queue, or denial
```

The gate receives resource information from the tool's real argument schema. It does not guess that an arbitrary positional argument is a path. A resource can carry more than one risk—for example, a Git worktree operation can involve both a repository and a new worktree path.

Shell command text is opaque to the permission gate. `bash.spawn` and `term.spawn` use their `cwd` argument as the structured filesystem scope for both approval and sandbox execution. A command that needs an external directory must set `cwd` to the narrowest required directory; an absolute path embedded only in `cmd` does not expand sandbox access. The platform temporary directory and Unix `/tmp` remain part of the controlled writable baseline.

The working directory resolves relative paths and omitted process directories; it is independent of the Flow's filesystem authority ceiling. Host-owned root Flows evaluate external targets through the current trust policy. Reckless and Eager Allow can authorize those targets without being rejected merely because they are outside the working directory. Temporary-directory access is governed by the tool's Tier and risk policy; the writable baseline does not make every write automatic. An explicitly directory-limited Flow must still stay within that ceiling, including for temporary files.

Process execution has a per-invocation boundary. Baseline automatic policy decisions and parent Flow approvals remain sandboxed. In Eager `escalation = "deny"`, an otherwise-Ask invocation whose only unresolved risk is process spawning runs inside the sandbox; the mode denies direct elevation rather than denying the process itself. An authenticated user approval bypasses the Atman process sandbox for that approved call only; Eager `escalation = "allow"` and Reckless mode also run the approved process directly. Explicit denials, other unresolved risks, and structural authority limits still apply.

The OS sandbox backend is available on macOS. Linux and Windows preserve the same configuration and permission model without installing an OS sandbox backend; automatic calls that require a sandbox report the existing controlled-execution error, while explicitly direct-authorized calls run directly.

The main policy actions are:

| Action | Result |
|---|---|
| `auto` | Run the tool without asking. |
| `ask` | Create a pending approval request and wait for a decision. |
| `deny` | Reject the invocation without running the tool. |

The permission decision is made once for each invocation. A cloned, invocation-specific context carries the resulting authorization to the tool, so one call cannot reuse another call's approval.

Embedded hosts can derive `EffectiveAuthority::read_only()` from an existing authority. It preserves the original directory ceiling and read/search eligibility in Tiers Zero and One, removes filesystem-write, subprocess, network, repository-mutation and permission-management capabilities, and cannot be widened by a later trust-mode change. The entry must also restrict its exposed tool names: these tiers include conversation and control operations in addition to reads. Model-returned calls continue to require the original request's tool-exposure receipt.

Waiting invocations consume the broker's committed result even when the last approval client disconnects or the completed request leaves bounded history. Missing clients cancel unresolved requests; they do not replace an existing approval, denial, or cancellation with a transport error.

## Versioned decisions

`permission.list` and `permission.get` include each request's `revision`. Individual `permission.approve`, `permission.deny`, and `permission.defer` calls require that value as `expected_revision`, alongside `request_id`. Re-read a request after a stale-version error; do not assume an earlier decision still applies after its approval target changes.

Request revision validation, authority checks, and commitment execute under the broker's lifecycle and request locks. Single decisions and atomic batch decisions use the same preparation and commitment path. Forwarding a request advances its revision before notifying the next approval target. A rejected stale decision does not create a grant, change the target, or resolve the waiting invocation.

Embedded controllers can use `PermissionBroker::resolve_at_revision` for the same contract. UI and daemon batch resolution retain their request-version and group-version checks.

## Peer approval access

An admitted `FlowClient` can inspect another instance's pending user approvals with `permission_requests` and approve or deny an observed request with `resolve_permission`. Both operations require an explicit `FlowAction::ApprovePermission` grant for that target, including across Sessions. Discovery, context access, execution control, creation relationships, and default-view selection do not grant this permission. A flow cannot approve its own requests.

The management grant editor exposes this capability as **Review approvals**. Management RPC grants use the `approve_permission` action.

The decision is committed by the requesting instance's existing broker. It validates the observed request revision, current caller lease and grant, and the reviewing flow's permission-management capability and resource authority. A paused, ended, or revoked reviewer cannot decide. Peer approvals retain the flow actor in audit records, remain sandboxed, and cannot create reusable tool or path exceptions. Concurrent user and peer decisions compete for the same request; only one can resolve the waiting invocation.

An undecided request remains available for user approval. Inspection and resolution do not transfer the request away from the user or notify reviewers.

`FlowClient::offer_permission_review` explicitly offers the calling invocation's pending request to a peer's durable inbox. The peer must have a live owner and an explicit review grant; an idle peer also needs an installed scheduler. Paused, closing, interrupted, finishing, or approval-blocked peers reject new offers. The operation does not open an unloaded instance or acquire its conversation lock.

An offer carries only the source handle, request ID, revision, and requesting run. It is internal context rather than a user request or an authorization. The peer inspects details through `permission.get` and decides through the existing versioned tools. Retrying the same offer returns its original acceptance without another notification or turn. Offers preserve the original user approval and do not register a substitute user client.

Persistent tool approval waits automatically offer pending requests to loaded peers with an explicit review grant. Candidates are considered once per invocation in Session/instance UUID order. The host observes durable commits, scheduling checkpoints, and grant changes without polling or acquiring a peer's conversation lock. A peer that pauses, loses access, waits on its own approval, or finishes without deciding yields to the next candidate. An idle scheduler that declines the notification also yields. An active peer can finish an admitted review even after its scheduler stops.

After candidate exhaustion, the request remains with connected user approval clients. Without a user client or a serving peer, the invocation is denied instead of waiting indefinitely. An accepted notification may remain in a paused peer's inbox; resumed reviewers must re-read the request and cannot decide a resolved or cancelled request. User and peer decisions remain subject to the same broker commitment, and an approved effect executes once.

The existing `permission.list`, `permission.get`, `permission.approve`, and `permission.deny` tools accept `target_handle` for peer review. Handles use an instance UUID in the current Session or `session-uuid/instance-uuid` across Sessions. Omitting the handle retains local request and group access. An explicit invalid or unauthorized handle never falls back to the local broker.

`permission.get` preserves the request ID and revision and adds `review` with the call intent, preview, resource provenance, risks, and policy information. The requesting instance applies its redaction policy before returning peer review detail. Treat that content as untrusted evidence, not instructions. The reported process boundary for a pending flow review is sandboxed.

```text
requests = permission.list(target_handle: target)
request = requests.requests[0]
detail = permission.get(target_handle: target, request_id: request.request_id)
permission.approve(target_handle: target, request_id: detail.request_id,
    expected_revision: detail.revision, scope: "current_call")
```

Peer decisions support individual requests and `current_call` scope only. Group operations, batch decisions, reusable exceptions, and forwarding through `permission.defer` reject an explicit peer target without changing the request. A reviewer can leave a request pending for the user.

## Trust modes

`TrustMode` selects the baseline Tier policy:

| Mode | Automatic baseline | Sandbox |
|---|---|---|
| `calm` | Only Tier 0 tools are automatic. | Enabled. |
| `steady` | Tier 0 and Tier 1 tools are automatic. | Enabled. |
| `eager` | Routine work is automatic; higher-risk policy decisions use `escalation`. | Enabled. |
| `reckless` | Tools are unrestricted after identity and authority checks. | Disabled. |

For process-spawning tools, "Enabled" is the automatic baseline rather than an immutable boundary. A real user approval selects direct execution for that invocation. Parent Flow approvals and baseline `auto` decisions select sandboxed execution. The selected boundary is retained by a scoped grant and shown in permission audit records.

The TUI may display a themed name and description for these modes. The persisted configuration uses the stable enum values above, not the themed labels.

`calm` and `steady` keep their fixed safety floor. Setting `escalation = "allow"` does not turn those modes into unrestricted execution.

## Escalation policy

`escalation` controls how `eager` handles a decision that is otherwise `ask`:

```toml
[trust]
mode = "eager"
escalation = "allow"
```

| `escalation` | Effect in `eager` |
|---|---|
| `deny` | Convert an escalation to `deny`. A process-only request falls back to sandboxed execution instead of being rejected. |
| `ask` | Keep the escalation as `ask`. This is the default. |
| `allow` | Convert an escalation to `auto`; process-spawning calls run directly. |

For example, `flow.spawn` is a Tier 2 tool. With `mode = "eager"` and `escalation = "allow"`, a normal Tier 2 spawn can pass the central gate without creating a pending approval. A risk override or the Flow's authority ceiling can still make the effective decision more restrictive.

`escalation` is not a separate approval queue and does not bypass the central broker. It is one input to the same policy resolver.

## Tier and risk overrides

Eager mode can override individual Tiers:

```toml
[trust.tiers.eager]
tier2 = "auto"
tier3 = "ask"
tier4 = "deny"
```

It can also override structured risks:

```toml
[trust.risks.eager]
network = "deny"
filesystem_write = "ask"
repository_mutation = "ask"
```

The effective controlled decision is the most restrictive result from the Tier and every declared risk:

```text
auto < ask < deny
```

A tool's provenance may include these risks:

- `workspace_external` — the resource is outside the managed workspace.
- `network` — the operation accesses a network resource.
- `irreversible` — the operation is difficult or impossible to undo.
- `filesystem_write` — the operation changes filesystem contents.
- `process_spawn` — the operation starts a process.
- `repository_mutation` — the operation changes repository state.

`workspace_external` and `outside_workspace` risk configuration describe a real resource boundary. They are not a legacy `outside` switch. The obsolete `[trust].outside` field is rejected instead of being silently ignored. Migrate it to the structured risk override that matches the resource, such as `workspace_external` or `network`; there is no global workspace-only permission switch.

## Daemon authentication and session ownership

The daemon HTTP API uses one static bearer token. A valid token and the owner-only Unix socket authenticate the same fixed principal, `local-daemon`. This single-operator control model allows both transports to manage the operator's sessions without granting access to other principals.

Even in this single-principal model, each live session is bound in the daemon's in-memory registry to the principal that created it. The permission RPCs `list_permission_requests`, `create_permission_group`, and `resolve_permission_requests` must match that owner before looking up or mutating the broker. A missing owner or a mismatched principal fails closed. The caller-provided session UUID is only a lookup key and is never treated as an authorization claim.

For persistent flows, all three RPCs accept `session_id` and `instance_id` at the top level. These identify the instance's own broker; changing the default flow does not redirect decisions. Returned requests include `instance_id`. An unknown or nil instance is rejected without falling back to a legacy session. Omitting `instance_id` retains the legacy live-session route and its ownership checks. Groups, request revisions, and scoped grants use the same broker validation on both routes.

The daemon retains an approval endpoint while its persistent session service is available. Every opened or spawned instance registers with that endpoint before execution can start. Closing one RPC connection does not remove the endpoint. Dropping the service disconnects its clients; when no approval client remains, pending user approvals are cancelled rather than left waiting. Retained flow handles cannot keep the endpoint alive.

Embedded hosts can provide the same ownership boundary with `RuntimeHost::with_services`:

```rust
use std::sync::Arc;
use atman_runtime::{
    flow_runtime::host::{HostServices, RuntimeHost},
    permission::PermissionClients,
};

let approvals = Arc::new(PermissionClients::default());
let host = RuntimeHost::with_services(HostServices {
    permission_clients: Arc::downgrade(&approvals),
    ..Default::default()
});
// Retain approvals while the transport can receive user decisions.
```

Endpoint registration establishes a decision transport; it does not approve any request or increase a flow's authority. Hosts without an approval endpoint continue to reject calls that require an unavailable user decision.

## Session state and global defaults

Permission state has three distinct roles:

1. **Global `TrustConfig`** — the default copied into a new Session.
2. **Session `trust.json`** — the durable permission snapshot owned by that Session.
3. **Runtime Session state** — the in-memory value read by the broker, tool gate, and TUI watch.

A new Session copies the current global configuration and writes its own `trust.json`. Opening an existing Session reads that Session's snapshot first. If an existing Session has no snapshot, the current global configuration initializes it. A malformed snapshot fails closed; it is not replaced silently with the global default.

Changing a Session's permission state updates the current Session and the global default. Other already-existing Sessions retain their own snapshots:

```text
Session A changes trust
  → A/trust.json is updated
  → global config.toml is updated
  → A runtime state publishes the new value
  → Session B is unchanged
```

The TUI stores its visual theme in `states.json`, but it does not use that file as the permission source. Mode and escalation changes go through the runtime control channel and are reflected in the TUI only after the Session publishes the new runtime snapshot.

## Runtime updates

The TUI sends a complete `TrustConfig` through `UpdateTrust`. The runtime update coordinator:

1. serializes the new Session snapshot;
2. writes the Session snapshot atomically;
3. writes the global configuration atomically;
4. rolls the Session snapshot back if the global write fails;
5. publishes the runtime watch value only after both writes succeed.

If the rollback also fails, the update returns an explicit inconsistent-persistence error. The runtime does not publish the new value while persistence is unresolved.

After a successful update, the next new tool invocation reads the current Session snapshot. No process restart is required.

Directly editing the global `config.toml` is not a live update channel for an already-running Session. It affects new Sessions and future bootstrap reads; an active Session continues using its own runtime snapshot until it receives an explicit runtime update.

## Pending requests and running Flows

Runtime updates apply to new invocations. They do not retroactively rewrite a request that is already waiting in the approval queue:

```text
Eager + Ask
  → tool call enters pending approval
  → switch to Allow
  → existing request remains pending
```

The user must still approve or deny that request, or cancel it through the normal flow controls.

A Flow also receives structural authority ceilings when it starts:

- a host-owned root Flow evaluates each new invocation using the current Session policy;
- tightening or loosening that policy changes the next root invocation without restarting the Flow;
- delegated execution, Tier, risk, shell, permission-management, and explicit filesystem ceilings remain intersected with the current policy and cannot be widened by a trust update;
- changing policy does not alter an already launched process or resolve an existing pending request.

Inheriting a working directory does not create a filesystem ceiling. Explicit directory narrowing creates a separate ceiling that descendants retain. Host-authorized worktree or project delegation binds the permitted destination while preserving the other capability limits.

Shell authority is intersected at every Flow boundary, including inline calls. Both the caller and callee must allow `contract { capabilities { shell: true } }`; a callee declaration cannot restore authority denied by its parent. Managed subagent entry and role Flows declare this capability so their existing process tools remain usable. Tool exposure is checked separately: the review role does not expose Bash, and a shell capability does not add tools to its model request.

## Fail-closed behavior

Controlled execution requires a complete trusted invocation context. Missing broker, Flow identity, registry binding, trust snapshot, or run identity causes denial. The runtime does not fall back to an older automatic-approval path.

A sandbox denial is final for that invocation. The runtime never converts it into an unrestricted retry. Direct execution is selected by the broker before the first launch only when the invocation carries user, Eager Allow, or Reckless authority.

## Audit events

Permission lifecycle changes are projected to both the persisted event log and the live stream. Request creation includes the initial approval target; a separate targeting record is emitted only after a real defer or retarget. The public event families cover request creation, targeting, deferral, approval, denial, and cancellation; group creation, update, and resolution; grant creation and expiry; and unrestricted execution.

Each record includes its stable request or grant identity, Flow ancestry, policy reference, authority actor, process boundary, scope, reason, and a bounded provenance summary. The summary includes resolved resource targets needed for audit without copying tool arguments or arbitrary payloads.

Records produced by one broker operation are emitted only after the operation commits. Failed atomic batches emit no partial records, and a successful approval is emitted immediately before its corresponding persistent grant. Lifecycle cleanup records identify a system component rather than inheriting the original decision actor.

Task registration, stopping, terminal status, and removal events are published in the same order as their registry state transitions. Cancellation publishes the stopping event before waking task cleanup; duplicate cancellation requests do not emit another transition. UI consumers can retain terminal history without returning stopped tasks to the active list.

New invocations write the canonical permission lifecycle only. Legacy tool approval and denial records remain replay-compatible for existing session logs.

The broker keeps complete state only for unresolved coordination work and retains the 256 most recent terminal requests for short-lived diagnostics. The persisted event log remains the durable complete history; a terminal request returns `RequestNotFound` from direct broker lookup after it leaves the recent window.

## Configuration reference

A complete Eager configuration can look like this:

```toml
[trust]
mode = "eager"
escalation = "ask"

[trust.tiers.eager]
tier2 = "auto"
tier3 = "ask"
tier4 = "deny"

[trust.risks.eager]
network = "deny"
filesystem_write = "ask"
repository_mutation = "ask"
```

Use `deny` for operations that must never run automatically, `ask` when the TUI should mediate the decision, and `allow` only when the resulting risk is acceptable for the Session. The central broker and the Flow authority ceiling remain in force for every setting.
