import type { HttpClient, RequestOptions } from "./client.ts";
import { canonicalUuid, flowSource } from "./cursor.ts";
import type { FlowSource } from "./cursor.ts";
import { ClientError } from "./errors.ts";
import { methods } from "./generated.ts";
import type { Dtos } from "./generated.ts";

export type PendingApproval = Readonly<{
  id: string;
  revision: number;
  requestingRun: string;
  tool: string;
  tier: Dtos["ApprovalTier"];
  toolUseId?: string;
  toolCallKey?: string;
  sameToolGrant: boolean;
  samePathGrant?: string;
  review: Dtos["FlowApprovalDetail"];
}>;
export type PendingApprovalGroup = Readonly<{
  id: string;
  revision: number;
  label: string;
  members: readonly string[];
}>;
export type ApprovalSnapshot = Readonly<{
  flow: Readonly<FlowSource>;
  state: "ready" | "unavailable" | "capacity";
  decisionReceipts: boolean;
  requests: readonly PendingApproval[];
  groups: readonly PendingApprovalGroup[];
}>;
export type ApprovalSelection =
  | Readonly<{ kind: "requests"; ids: readonly string[] }>
  | Readonly<{ kind: "group"; id: string }>;
export type ApprovalAction = "allow_once" | "allow_tool" | "allow_path" | "deny" | "cancel_request";
export type ScopedApprovalAction = "allow_once" | "deny" | "cancel_request";
export type ScopedApprovalCommand = Readonly<{
  request: Dtos["ApplyFlowApprovalRequest"];
  requestIds: readonly string[];
  action: ScopedApprovalAction;
}>;
export type ApprovalAcknowledgement = Readonly<{
  flow: Readonly<FlowSource>;
  action: ApprovalAction;
  requestIds: readonly string[];
  state: "acknowledged";
  decisions: readonly Readonly<{ requestId: string; decisionId: string }>[];
}>;

const actions = { allow_once: "approve", allow_tool: "approve", allow_path: "approve", deny: "deny", cancel_request: "defer" } as const;
const outcome = { allow_once: "approved", allow_tool: "approved", allow_path: "approved", deny: "denied", cancel_request: "cancelled" } as const;

function reject(reason: string): never {
  throw new ClientError("request", reason, "not_sent");
}

function checkedId(value: string): string {
  try { return canonicalUuid(value); }
  catch { return reject("Expected an original approval identity"); }
}

function revision(value: number): number {
  if (!Number.isSafeInteger(value) || value < 0) throw new ClientError("source", "Approval revision is invalid");
  return value;
}

function members(ids: readonly string[]): readonly string[] {
  const values = ids.map(canonicalUuid);
  if (values.length === 0 || new Set(values).size !== values.length)
    throw new ClientError("source", "Approval members are absent or duplicated");
  return Object.freeze(values);
}

function group(value: Dtos["PermissionGroupView"]): PendingApprovalGroup {
  return Object.freeze({ id: canonicalUuid(value.group_id), revision: revision(value.revision),
    label: value.label, members: members(value.request_ids) });
}

function exactAcknowledgement(expected: readonly string[], actual: readonly string[]): void {
  const selected = new Set(expected);
  if (actual.length !== selected.size ||
    new Set(actual).size !== actual.length || actual.some((id) => !selected.has(id)))
    throw new ClientError("source", "Approval reply does not match the exact selected requests", "unknown");
}

function commandReply<T>(check: () => T): T {
  try { return check(); }
  catch (error) {
    if (error instanceof ClientError && error.outcome === "unknown") throw error;
    throw new ClientError("source", "Approval command reply could not be bound to its original selection", "unknown");
  }
}

function acknowledgement(flow: Readonly<FlowSource>, selected: readonly string[], action: ApprovalAction,
  rows: readonly { request_id: string; decision?: Dtos["ApprovalDecisionView"] | null }[]): ApprovalAcknowledgement {
  exactAcknowledgement(selected, rows.map((row) => canonicalUuid(row.request_id)));
  const decisions = rows.map((row) => {
    if (row.decision?.action !== outcome[action])
      throw new ClientError("source", "Approval reply does not prove the original decision", "unknown");
    return Object.freeze({ requestId: canonicalUuid(row.request_id), decisionId: canonicalUuid(row.decision.decision_id) });
  });
  if (new Set(decisions.map((decision) => decision.decisionId)).size !== decisions.length)
    throw new ClientError("source", "Approval reply repeats a decision ID", "unknown");
  return Object.freeze({ flow, action, requestIds: Object.freeze([...selected]),
    state: "acknowledged", decisions: Object.freeze(decisions) });
}

function scopedAction(action: ApprovalAction): action is ScopedApprovalAction {
  return action === "allow_once" || action === "deny" || action === "cancel_request";
}

/** The current User broker for one authenticated, explicit native Flow address. */
export class FlowApprovals {
  readonly #target: Readonly<Dtos["PermissionTarget"]>;
  readonly #address: Readonly<FlowSource>;
  #latest: ApprovalSnapshot | undefined;
  #observing = 0;

  constructor(private readonly transport: HttpClient, address: FlowSource) {
    this.#address = flowSource(address);
    this.#target = Object.freeze({ ...this.#address });
  }

  async observe(options: RequestOptions = {}): Promise<ApprovalSnapshot> {
    const operation = ++this.#observing;
    try {
      const result = await this.transport.request(methods["flow.approval_queue"], { flow: this.#address }, options);
      const from = flowSource(result.flow);
      if (from.session_id !== this.#address.session_id || from.instance_id !== this.#address.instance_id)
        throw new ClientError("source", "Approval queue belongs to another Flow");
      if (result.state !== "ready") {
        if (this.#observing === operation) this.#latest = undefined;
        return Object.freeze({ flow: this.#address, state: result.state, decisionReceipts: false, requests: [], groups: [] });
      }
      const requests = result.requests.map((row) => {
        const review = Object.freeze({ ...row.detail,
          risks: Object.freeze([...row.detail.risks]), targets: Object.freeze([...row.detail.targets]),
        });
        if ((review.execution_boundary === "direct") !== review.risks.includes("process_spawn"))
          throw new ClientError("source", "User review has inconsistent process execution detail");
        return Object.freeze({ id: canonicalUuid(row.request_id), revision: revision(row.revision),
          requestingRun: canonicalUuid(row.requesting_run_id), tool: row.tool, tier: row.tier, review,
          sameToolGrant: row.same_tool_grant === true && row.tool.length > 0,
          ...(row.same_tool_grant === true && row.tool && row.workspace_relative_grant_path
            ? { samePathGrant: row.workspace_relative_grant_path } : {}),
          ...(row.tool_use_id == null ? {} : { toolUseId: row.tool_use_id }),
          ...(row.tool_call_key == null ? {} : { toolCallKey: row.tool_call_key }) });
      });
      const ids = new Set(requests.map((request) => request.id));
      if (ids.size !== requests.length) throw new ClientError("source", "Approval request is duplicated");
      const groups = result.groups.map(group);
      if (new Set(groups.map((entry) => entry.id)).size !== groups.length ||
        groups.some((entry) => entry.members.some((id) => !ids.has(id))))
        throw new ClientError("source", "Approval group differs from the current User queue");
      const snapshot = Object.freeze({ flow: this.#address, state: result.state, decisionReceipts: result.decision_receipts === true, requests: Object.freeze(requests), groups: Object.freeze(groups) });
      if (this.#observing === operation && !options.signal?.aborted) this.#latest = snapshot;
      return snapshot;
    } catch (error) {
      if (this.#observing === operation) this.#latest = undefined;
      throw error;
    }
  }

  #requestSelection(snapshot: ApprovalSnapshot, ids: readonly string[]) {
    if (snapshot !== this.#latest) return reject("Approval review is out of date");
    const request_ids = ids.map(checkedId);
    if (new Set(request_ids).size !== request_ids.length || request_ids.length === 0)
      return reject("Approval selection must contain distinct requests");
    const rows = new Map(snapshot.requests.map((row) => [row.id, row.revision]));
    const expected_request_revisions = Object.fromEntries(request_ids.map((id) => {
      const observed = rows.get(id);
      if (observed === undefined) return reject("Approval request is outside the current User queue");
      return [id, observed];
    }));
    return { request_ids, expected_request_revisions };
  }

  #retireReview(): void {
    this.#latest = undefined;
    ++this.#observing;
  }

  async createGroup(snapshot: ApprovalSnapshot, ids: readonly string[], label: string,
    options: RequestOptions = {}): Promise<PendingApprovalGroup> {
    const selected = this.#requestSelection(snapshot, ids);
    this.#retireReview();
    const reply = await this.transport.request(methods.create_permission_group,
      { ...this.#target, ...selected, label }, options);
    return commandReply(() => {
      const created = group(reply);
      exactAcknowledgement(selected.request_ids, created.members);
      if (created.label !== label || snapshot.groups.some((entry) => entry.id === created.id))
        throw new ClientError("source", "Approval group reply lacks its new identity and original label", "unknown");
      return created;
    });
  }

  #decisionSelection(snapshot: ApprovalSnapshot, selection: ApprovalSelection, action: ApprovalAction) {
    if (!Object.hasOwn(actions, action)) return reject("Approval action is unavailable");
    if (snapshot !== this.#latest) return reject("Approval review is out of date");
    if (!snapshot.decisionReceipts) return reject("The owner does not publish approval decision receipts");
    let selector: { request_ids: string[]; expected_request_revisions: Record<string, number> }
      | { group_id: string; expected_group_revision: number };
    let selected: readonly string[];
    if (selection.kind === "group") {
      const id = checkedId(selection.id);
      const observed = snapshot.groups.find((entry) => entry.id === id);
      if (observed === undefined) return reject("Approval group is outside the current User queue");
      selected = observed.members;
      selector = { group_id: observed.id, expected_group_revision: observed.revision };
    } else {
      const requests = this.#requestSelection(snapshot, selection.ids);
      selected = requests.request_ids;
      selector = requests;
    }
    const ids = new Set(selected);
    if (actions[action] === "approve" && snapshot.requests.some((row) => ids.has(row.id) && row.review.incomplete))
      return reject("Review the complete approval on the host before allowing it");
    return { selector, selected };
  }

  async resolve(snapshot: ApprovalSnapshot, selection: ApprovalSelection, action: ApprovalAction,
    options: RequestOptions = {}): Promise<ApprovalAcknowledgement> {
    const { selector, selected } = this.#decisionSelection(snapshot, selection, action);
    let scope: Dtos["PermissionRpcScope"] | undefined;
    if (action === "allow_once") scope = "current_call";
    if (action === "allow_tool" || action === "allow_path") {
      const row = selection.kind === "requests" && selected.length === 1
        ? snapshot.requests.find(({ id }) => id === selected[0]) : undefined;
      if (!row?.sameToolGrant) return reject("This review does not offer a same-tool grant");
      const exact = { run_id: row.requestingRun, tool_name: row.tool };
      if (action === "allow_tool") scope = { child_run_same_tool: exact };
      else {
        if (!row.samePathGrant) return reject("This review does not offer a same-path grant");
        scope = { child_run_same_path_rule: { ...exact, workspace_relative_path: row.samePathGrant } };
      }
    }
    this.#retireReview();
    const reply = await this.transport.request(methods.resolve_permission_requests, {
      ...this.#target, selector, action: actions[action],
      ...(scope === undefined ? {} : { scope }),
    }, options);
    return commandReply(() => acknowledgement(this.#address, selected, action, reply.resolutions));
  }

  /** Capture the exact review for delivery or later receipt lookup. */
  prepareScoped(snapshot: ApprovalSnapshot, selection: ApprovalSelection, action: ScopedApprovalAction,
    operationId: string): ScopedApprovalCommand {
    if (!scopedAction(action)) return reject("This approval only authorizes decisions for the current call");
    const { selector, selected } = this.#decisionSelection(snapshot, selection, action);
    let target: Dtos["FlowApprovalSelection"];
    if ("group_id" in selector) {
      target = { kind: "group", group_id: selector.group_id, revision: selector.expected_group_revision };
    } else {
      const id = selected[0];
      if (!id || selected.length !== 1) return reject("Review a single request or an existing approval group");
      target = { kind: "request", request_id: id, revision: selector.expected_request_revisions[id]! };
    }
    const operation = Object.freeze({ flow: this.#address, operation_id: checkedId(operationId) });
    const request = Object.freeze({ operation, selection: Object.freeze(target), action: actions[action] });
    return Object.freeze({ request, requestIds: Object.freeze([...selected]), action });
  }

  async applyScoped(command: ScopedApprovalCommand, options: RequestOptions = {}): Promise<ApprovalAcknowledgement> {
    const captured = this.#captureScoped(command);
    this.#retireReview();
    const reply = await this.transport.request(methods["flow.approval_apply"], captured.request, options);
    return commandReply(() => {
      const receipt = this.#scopedReceipt(captured, reply);
      if (!receipt) throw new ClientError("source", "Approval decision receipt is missing", "unknown");
      return receipt;
    });
  }

  async scopedStatus(command: ScopedApprovalCommand, options: RequestOptions = {}): Promise<ApprovalAcknowledgement | undefined> {
    const captured = this.#captureScoped(command);
    const reply = await this.transport.request(methods["flow.approval_status"], captured.request.operation, options);
    return commandReply(() => this.#scopedReceipt(captured, reply));
  }

  #captureScoped(command: ScopedApprovalCommand): ScopedApprovalCommand {
    const { request, requestIds, action } = structuredClone(command);
    const flow = flowSource(request.operation.flow);
    if (flow.session_id !== this.#address.session_id || flow.instance_id !== this.#address.instance_id)
      return reject("Approval command belongs to another Flow");
    if (!scopedAction(action) || request.action !== actions[action])
      return reject("Approval command does not match the original action");
    const ids = members(requestIds);
    if (request.selection.kind === "request" && (ids.length !== 1 || ids[0] !== checkedId(request.selection.request_id)))
      return reject("Approval command does not match the original request");
    return { request: { ...request, operation: { flow, operation_id: checkedId(request.operation.operation_id) } },
      requestIds: ids, action };
  }

  #scopedReceipt(command: ScopedApprovalCommand, reply: Dtos["FlowApprovalOperationResponse"]): ApprovalAcknowledgement | undefined {
    const flow = flowSource(reply.operation.flow);
    if (flow.session_id !== this.#address.session_id || flow.instance_id !== this.#address.instance_id ||
      canonicalUuid(reply.operation.operation_id) !== command.request.operation.operation_id)
      throw new ClientError("source", "Approval reply belongs to another operation", "unknown");
    if (reply.decisions != null) return acknowledgement(flow, command.requestIds, command.action, reply.decisions);
  }
}
