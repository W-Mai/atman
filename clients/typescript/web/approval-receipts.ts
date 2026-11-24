import { canonicalUuid, decodeDto } from "../src/index.ts";
import type { FlowSource, ScopedApprovalCommand } from "../src/index.ts";

export const maxPendingApprovals = 16;
const maxMembers = 64;
const maxArchiveLength = 65536;
const flowKey = ({ session_id, instance_id }: FlowSource) => `${session_id}/${instance_id}`;
type StoragePort = Pick<Storage, "getItem" | "setItem" | "removeItem">;

function restore(value: { request: unknown; requestIds: unknown }, session: string): ScopedApprovalCommand | undefined {
  const request = decodeDto("ApplyFlowApprovalRequest", value.request);
  const { operation, selection } = request;
  if (canonicalUuid(operation.flow.session_id) !== session || !Number.isSafeInteger(selection.revision) ||
      !Array.isArray(value.requestIds) || value.requestIds.length < 1 || value.requestIds.length > maxMembers) return;
  const requestIds = value.requestIds.map(canonicalUuid);
  if (new Set(requestIds).size !== requestIds.length) return;
  const selected = canonicalUuid(selection.kind === "group" ? selection.group_id : selection.request_id);
  if (selection.kind === "request" && (requestIds.length !== 1 || requestIds[0] !== selected)) return;
  return {
    request: { action: request.action, operation: { flow: { session_id: session,
      instance_id: canonicalUuid(operation.flow.instance_id) }, operation_id: canonicalUuid(operation.operation_id) }, selection },
    requestIds,
    action: request.action === "approve" ? "allow_once" : request.action === "deny" ? "deny" : "cancel_request",
  };
}

/** Keep only receipt lookup identities, scoped to a device and its shared Session in this tab. */
export class ApprovalReceipts {
  readonly #key: string;
  readonly #requests = new Map<string, ScopedApprovalCommand>();

  constructor(device: string, session: string, private readonly storage?: StoragePort) {
    const owner = canonicalUuid(session);
    this.#key = `atman.device.approvals.v1.${canonicalUuid(device)}.${owner}`;
    try {
      const raw = storage?.getItem(this.#key);
      if (!raw || raw.length > maxArchiveLength) return;
      const values: unknown = JSON.parse(raw);
      if (!Array.isArray(values)) return;
      for (const value of values.slice(0, maxPendingApprovals)) {
        try {
          const command = restore(value, owner);
          if (command) this.#requests.set(flowKey(command.request.operation.flow), command);
        } catch { /* Ignore a malformed receipt identity. */ }
      }
    } catch { /* Keep this tab usable when storage is unavailable. */ }
  }

  get(flow: FlowSource): ScopedApprovalCommand | undefined { return this.#requests.get(flowKey(flow)); }

  add(command: ScopedApprovalCommand): boolean {
    const key = flowKey(command.request.operation.flow);
    if (this.#requests.has(key) || this.#requests.size >= maxPendingApprovals) return false;
    this.#requests.set(key, command);
    this.#persist();
    return true;
  }

  remove(command: ScopedApprovalCommand): void {
    const key = flowKey(command.request.operation.flow);
    if (this.#requests.get(key) !== command) return;
    this.#requests.delete(key);
    this.#persist();
  }

  #persist(): void {
    try {
      if (this.#requests.size) this.storage?.setItem(this.#key, JSON.stringify([...this.#requests.values()]));
      else this.storage?.removeItem(this.#key);
    } catch { /* The original operation is still available to this tab in memory. */ }
  }
}
