import { ClientError } from "../src/index.ts";
import type { FlowClient } from "../src/index.ts";
import type {
  ApprovalAcknowledgement,
  ApprovalAction,
  ApprovalSelection,
  ApprovalSnapshot,
  PendingApproval,
  ScopedApprovalCommand,
} from "../src/approvals.ts";
import { maxPendingApprovals, type ApprovalReceipts } from "./approval-receipts.ts";

type Flow = Pick<FlowClient, "address" | "approvals">;
export type ApprovalTarget = Readonly<{ flow: Flow; label: string }>;
type Notice =
  | { readonly state: "settled"; readonly receipt: ApprovalAcknowledgement }
  | { readonly state: "unknown" | "not_sent" | "capacity" };
type Seal = Readonly<{
  selection: ApprovalSelection;
  revisionKey: string;
  requests: readonly PendingApproval[];
  label: string;
}>;

export type ApprovalView = Readonly<{
  key: string;
  label: string;
  snapshot: ApprovalSnapshot | undefined;
  notice: Notice | undefined;
  review: Seal | undefined;
  reading: boolean;
  sending: boolean;
  checking: boolean;
  pending: boolean;
  currentCallOnly: boolean;
  failed: boolean;
  attention: boolean;
  writable: boolean;
  canDecide: boolean;
}>;

const keyOf = ({ address }: Flow) => `${address.session_id}/${address.instance_id}`;
const actions = new Set<ApprovalAction>(["allow_once", "allow_tool", "allow_path", "deny", "cancel_request"]);

function seal(snapshot: ApprovalSnapshot, selection: ApprovalSelection): Seal | undefined {
  if (selection.kind === "group") {
    const group = snapshot.groups.find(({ id }) => id === selection.id);
    if (!group) return;
    const rows = new Map(snapshot.requests.map((row) => [row.id, row]));
    const members = group.members.map((id) => rows.get(id));
    if (!members.every((member) => member !== undefined)) return;
    return {
      selection: { kind: "group", id: group.id },
      revisionKey: JSON.stringify([group.id, group.revision,
        ...members.map(({ id, revision, requestingRun, toolUseId }) => [id, revision, requestingRun, toolUseId])]),
      requests: members,
      label: group.label || "Approval group",
    };
  }
  // Multi-request authorization uses a displayed group; single rows retain their own exact revision.
  if (selection.ids.length !== 1) return;
  const member = snapshot.requests.find(({ id }) => id === selection.ids[0]);
  if (!member) return;
  return {
    selection: { kind: "requests", ids: [member.id] },
    revisionKey: JSON.stringify([member.id, member.revision, member.requestingRun, member.toolUseId, member.sameToolGrant, member.samePathGrant]),
    requests: [member],
    label: member.review.intent || member.tool || "Tool call",
  };
}

/** Observe the verified focused Flow without owning its permission client or execution. */
export class ApprovalActivity {
  #target: ApprovalTarget | undefined;
  #connection: AbortController | undefined;
  #read: AbortController | undefined;
  #delayed: ReturnType<typeof setTimeout> | undefined;
  #dirty = false;
  #snapshot: ApprovalSnapshot | undefined;
  #retiredSnapshot: ApprovalSnapshot | undefined;
  #review: Seal | undefined;
  #failed = false;
  #command = false;
  #receipts: ApprovalReceipts | undefined;
  #view: ApprovalView | undefined;
  #viewGeneration = 0;
  #operations = new Map<string, "sending" | "checking">();
  #notices = new Map<string, Notice>();

  constructor(private readonly present: (view: ApprovalView | undefined) => void) {}

  connect(command: boolean | ApprovalReceipts): void {
    this.disconnect();
    this.#connection = new AbortController();
    this.#command = command !== false;
    this.#receipts = typeof command === "boolean" ? undefined : command;
  }

  disconnect(): void {
    this.#connection?.abort();
    this.#connection = undefined;
    this.#command = false;
    this.#receipts = undefined;
    this.#operations.clear();
    this.#notices.clear();
    this.select(undefined);
  }

  select(target: ApprovalTarget | undefined): void {
    if (!this.#connection) target = undefined;
    if (target === this.#target) return;
    this.#read?.abort();
    this.#read = undefined;
    clearTimeout(this.#delayed);
    this.#delayed = undefined;
    this.#dirty = false;
    this.#target = target;
    this.#viewGeneration++;
    this.#snapshot = undefined;
    this.#retiredSnapshot = undefined;
    this.#review = undefined;
    this.#failed = false;
    if (!target) { this.#show(); return; }
    const command = this.#receipts?.get(target.flow.address);
    if (command && !this.#operations.has(keyOf(target.flow))) void this.#deliver(target, "checking", command);
    this.refresh();
  }

  reviewTool(flow: Flow, view: ApprovalView, request: PendingApproval): ApprovalView | undefined {
    if (flow !== this.#target?.flow || view !== this.#view || this.#snapshot?.state !== "ready" ||
        !request.toolCallKey || this.#snapshot.requests.filter(({ toolCallKey }) => toolCallKey === request.toolCallKey).length !== 1 ||
        !this.#snapshot.requests.includes(request)) return;
    if (this.review(view, { kind: "requests", ids: [request.id] })) return this.#view;
  }

  refresh(): void {
    const target = this.#target;
    if (!target || !this.#connection) return;
    clearTimeout(this.#delayed);
    this.#delayed = undefined;
    if (this.#operations.get(keyOf(target.flow)) === "sending") return;
    if (this.#read) {
      this.#dirty = true;
      return;
    }
    void this.#inspect(target);
  }

  invalidate(flow: Flow): void {
    if (flow !== this.#target?.flow) return;
    // The debounced refresh retires only this tab's observation. The broker request stays live.
    if (this.#read) this.#dirty = true;
    else if (this.#delayed === undefined) this.#delayed = setTimeout(() => this.refresh(), 150);
  }

  review(view: ApprovalView, selection: ApprovalSelection): boolean {
    if (view !== this.#view || this.#failed || this.#snapshot?.state !== "ready" || this.#snapshot === this.#retiredSnapshot) return false;
    const review = seal(this.#snapshot, selection);
    if (!review) return false;
    this.#review = review;
    this.#show();
    return true;
  }

  dismiss(view: ApprovalView): void {
    if (view !== this.#view || !this.#review) return;
    this.#review = undefined;
    this.#show();
  }

  issue(view: ApprovalView, action: ApprovalAction): void {
    const { snapshot, review } = view;
    const target = this.#target;
    const connection = this.#connection;
    if (view !== this.#view || !target || !connection || !actions.has(action) || !view.canDecide ||
      !snapshot || !review || this.#operations.size >= maxPendingApprovals) return;
    if (action.startsWith("allow_") && review.requests.some(({ review }) => review.incomplete)) return;
    const key = keyOf(target.flow);
    const receipts = this.#receipts;
    if (receipts) {
      if (action === "allow_path" || action === "allow_tool") return;
      try {
        const command = target.flow.approvals.prepareScoped(snapshot, review.selection, action, crypto.randomUUID());
        if (receipts.add(command)) {
          void this.#deliver(target, "sending", command);
          return;
        }
        this.#notice(key, { state: "capacity" });
      } catch {
        this.#notice(key, { state: "not_sent" });
      }
      this.#show();
      return;
    }
    void this.#deliver(target, "sending", undefined,
      () => target.flow.approvals.resolve(snapshot, review.selection, action, { signal: connection.signal }));
  }

  check(view: ApprovalView): void {
    const target = this.#target;
    if (!target || view !== this.#view || view.sending || view.checking) return;
    const command = this.#receipts?.get(target.flow.address);
    if (command) void this.#deliver(target, "checking", command);
  }

  dismissPending(view: ApprovalView): void {
    const target = this.#target;
    if (!target || view !== this.#view || view.sending || view.checking) return;
    const command = this.#receipts?.get(target.flow.address);
    if (command) {
      this.#receipts?.remove(command);
      this.#notices.delete(keyOf(target.flow));
      this.#show();
      this.refresh();
    }
  }

  #notice(key: string, notice: Notice): void {
    this.#notices.delete(key);
    this.#notices.set(key, notice);
    const oldest = this.#notices.keys().next().value;
    if (oldest !== undefined && this.#notices.size > maxPendingApprovals) this.#notices.delete(oldest);
  }

  async #deliver(target: ApprovalTarget, phase: "sending" | "checking", command?: ScopedApprovalCommand,
    resolve?: () => Promise<ApprovalAcknowledgement>): Promise<void> {
    const connection = this.#connection;
    const key = keyOf(target.flow);
    if (!connection || this.#operations.has(key)) return;
    if (phase === "sending") {
      // The displayed revision remains actionable during a refresh. Retire the read before the SDK consumes it.
      this.#read?.abort();
      this.#read = undefined;
      this.#dirty = false;
      this.#retiredSnapshot = this.#snapshot;
    }
    this.#operations.set(key, phase);
    this.#notices.delete(key);
    this.#show();
    const current = () => this.#connection === connection && !connection.signal.aborted;
    try {
      const options = { signal: connection.signal };
      const receipt = command ? await (phase === "sending" ? target.flow.approvals.applyScoped(command, options)
        : target.flow.approvals.scopedStatus(command, options)) : await resolve?.();
      if (!current()) return;
      if (receipt) {
        if (command) this.#receipts?.remove(command);
        this.#notice(key, { state: "settled", receipt });
      } else this.#notice(key, { state: "unknown" });
    } catch (error) {
      if (!current()) return;
      const unsent = phase === "sending" && error instanceof ClientError && error.outcome === "not_sent";
      if (command && unsent) this.#receipts?.remove(command);
      this.#notice(key, { state: unsent ? "not_sent" : "unknown" });
    } finally {
      if (current()) {
        this.#operations.delete(key);
        if (this.#target && keyOf(this.#target.flow) === key) {
          if (phase === "sending") this.refresh();
          else this.#show();
        }
      }
    }
  }

  #show(): void {
    const target = this.#target;
    if (!target) {
      this.#view = undefined;
    } else {
      const key = keyOf(target.flow);
      const snapshot = this.#snapshot;
      const operation = this.#operations.get(key);
      const reading = this.#read !== undefined;
      const pending = this.#receipts?.get(target.flow.address) !== undefined;
      const writable = this.#command && snapshot?.decisionReceipts === true;
      this.#view = Object.freeze({
        key: `${key}/${this.#viewGeneration}`, label: target.label, snapshot, reading, pending,
        sending: operation === "sending", checking: operation === "checking", currentCallOnly: this.#receipts !== undefined,
        notice: this.#notices.get(key),
        review: this.#review,
        failed: this.#failed,
        attention: !this.#failed && snapshot?.state === "ready" && snapshot.requests.length > 0,
        writable,
        canDecide: writable && snapshot !== this.#retiredSnapshot && this.#review !== undefined && !operation && !pending && !this.#failed,
      });
    }
    this.present(this.#view);
  }

  async #inspect(target: ApprovalTarget): Promise<void> {
    const read = new AbortController();
    this.#read = read;
    this.#show();
    try {
      const snapshot = await target.flow.approvals.observe({ signal: read.signal });
      if (this.#read !== read) return;
      this.#snapshot = snapshot;
      this.#retiredSnapshot = undefined;
      this.#failed = false;
      if (this.#review !== undefined) {
        const replacement = snapshot.state === "ready" ? seal(snapshot, this.#review.selection) : undefined;
        this.#review = replacement?.revisionKey === this.#review.revisionKey ? replacement : undefined;
      }
    } catch {
      if (this.#read !== read) return;
      this.#snapshot = undefined;
      this.#review = undefined;
      this.#failed = true;
    } finally {
      if (this.#read === read) {
        this.#read = undefined;
        this.#show();
        if (this.#dirty) {
          this.#dirty = false;
          this.invalidate(target.flow);
        }
      }
    }
  }
}
