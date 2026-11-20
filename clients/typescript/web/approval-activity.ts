import { ClientError } from "../src/index.ts";
import type { FlowClient } from "../src/index.ts";
import type {
  ApprovalAcknowledgement,
  ApprovalAction,
  ApprovalSelection,
  ApprovalSnapshot,
  PendingApproval,
} from "../src/approvals.ts";

type Flow = Pick<FlowClient, "address" | "approvals">;
export type ApprovalTarget = Readonly<{ flow: Flow; label: string }>;
type Notice =
  | { readonly state: "settled"; readonly receipt: ApprovalAcknowledgement }
  | { readonly state: "unknown" | "not_sent" };
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
  failed: boolean;
  attention: boolean;
  writable: boolean;
  canDecide: boolean;
}>;

const keyOf = ({ address }: Flow) => `${address.session_id}/${address.instance_id}`;
const MAX_PENDING = 16;
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
  #review: Seal | undefined;
  #failed = false;
  #command = false;
  #view: ApprovalView | undefined;
  #viewGeneration = 0;
  #writes = new Set<string>();
  #notices = new WeakMap<ApprovalTarget, Notice>();

  constructor(private readonly present: (view: ApprovalView | undefined) => void) {}

  connect(command: boolean): void {
    this.disconnect();
    this.#connection = new AbortController();
    this.#command = command;
  }

  disconnect(): void {
    this.#connection?.abort();
    this.#connection = undefined;
    this.#command = false;
    this.#writes.clear();
    this.#notices = new WeakMap();
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
    this.#review = undefined;
    this.#failed = false;
    if (target) this.refresh();
    else this.#show();
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
    if (view !== this.#view || this.#failed || this.#read || this.#snapshot?.state !== "ready") return false;
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
      !snapshot || !review || this.#writes.size >= MAX_PENDING) return;
    if (action.startsWith("allow_") && review.requests.some(({ review }) => review.incomplete)) return;
    const key = keyOf(target.flow);
    this.#writes.add(key);
    this.#notices.delete(target);
    this.#show();
    const current = () => this.#connection === connection && !connection.signal.aborted;
    void target.flow.approvals.resolve(snapshot, review.selection, action, { signal: connection.signal })
      .then((receipt) => {
        if (current()) this.#notices.set(target, { state: "settled", receipt });
      })
      .catch((error: unknown) => {
        if (current()) this.#notices.set(target, {
          state: error instanceof ClientError && error.outcome === "not_sent" ? "not_sent" : "unknown",
        });
      })
      .finally(() => {
        if (!current()) return;
        this.#writes.delete(key);
        if (this.#target && keyOf(this.#target.flow) === key) this.refresh();
      });
  }

  #show(): void {
    const target = this.#target;
    if (!target) {
      this.#view = undefined;
    } else {
      const key = keyOf(target.flow);
      const snapshot = this.#snapshot;
      const sending = this.#writes.has(key);
      const reading = this.#read !== undefined;
      const writable = this.#command && snapshot?.decisionReceipts === true;
      this.#view = Object.freeze({
        key: `${key}/${this.#viewGeneration}`, label: target.label, snapshot, reading, sending,
        notice: this.#notices.get(target),
        review: this.#review,
        failed: this.#failed,
        attention: !this.#failed && snapshot?.state === "ready" && snapshot.requests.length > 0,
        writable,
        canDecide: writable && this.#review !== undefined && !sending && !reading && !this.#failed,
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
