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

Shell command text is opaque to the permission gate. `bash.spawn` and `term.spawn` use their `cwd` argument as the structured filesystem scope for both approval and sandbox execution. A command that needs an external directory must set `cwd` to the narrowest required directory; an absolute path embedded only in `cmd` does not expand sandbox access.

The main policy actions are:

| Action | Result |
|---|---|
| `auto` | Run the tool without asking. |
| `ask` | Create a pending approval request and wait for a decision. |
| `deny` | Reject the invocation without running the tool. |

The permission decision is made once for each invocation. A cloned, invocation-specific context carries the resulting authorization to the tool, so one call cannot reuse another call's approval.

## Trust modes

`TrustMode` selects the baseline Tier policy:

| Mode | Automatic baseline | Sandbox |
|---|---|---|
| `calm` | Only Tier 0 tools are automatic. | Enabled. |
| `steady` | Tier 0 and Tier 1 tools are automatic. | Enabled. |
| `eager` | Routine work is automatic; higher-risk policy decisions use `escalation`. | Enabled. |
| `reckless` | Tools are unrestricted after identity and authority checks. | Disabled. |

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
| `deny` | Convert an escalation to `deny`. |
| `ask` | Keep the escalation as `ask`. This is the default. |
| `allow` | Convert an escalation to `auto`. |

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

The daemon HTTP API uses one static bearer token. A valid token authenticates as the fixed principal `authenticated-daemon-client`; it is a single-operator control model, not a multi-user identity provider. Unix-socket requests use the local principal `local-daemon`.

Even in this single-principal model, each live session is bound in the daemon's in-memory registry to the principal that created it. The permission RPCs `list_permission_requests`, `create_permission_group`, and `resolve_permission_requests` must match that owner before looking up or mutating the broker. A missing owner or a mismatched principal fails closed. The caller-provided session UUID is only a lookup key and is never treated as an authorization claim.

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

A Flow also receives an authority ceiling when it starts. The ceiling preserves the most restrictive startup policy for that Flow:

- tightening the Session policy affects the next invocation immediately;
- loosening the Session policy cannot expand an already-running Flow;
- switching a running controlled Flow to `reckless` does not remove its existing sandbox or authority ceiling;
- a new root Flow can inherit the newly configured policy.

This prevents a running Flow from creating a broad authority and then widening itself through a runtime trust update.

## Fail-closed behavior

Controlled execution requires a complete trusted invocation context. Missing broker, Flow identity, registry binding, trust snapshot, or run identity causes denial. The runtime does not fall back to an older automatic-approval path.

A strict sandbox denial is final for that invocation. The runtime never converts it into an unrestricted retry.

## Audit events

Permission lifecycle changes are projected to both the persisted event log and the live stream. The public event families cover request creation, targeting, deferral, approval, denial, and cancellation; group creation, update, and resolution; grant creation and expiry; and unrestricted execution.

Each record includes its stable request or grant identity, Flow ancestry, policy reference, authority actor, scope, reason, and a bounded provenance summary. The summary includes resolved resource targets needed for audit without copying tool arguments or arbitrary payloads.

Records produced by one broker operation are emitted only after the operation commits. Failed atomic batches emit no partial records, and a successful approval is emitted immediately before its corresponding persistent grant. Lifecycle cleanup records identify a system component rather than inheriting the original decision actor.

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
