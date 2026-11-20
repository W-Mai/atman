import { ClientError, RemoteError } from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import { sameHistoryHead } from "./history-controller.ts";

export type InboxPage = Dtos["ReadFlowInboxResponse"];
export type InboxItem = Dtos["FlowInboxItem"];
type Change = Dtos["FlowInboxChange"];
type Request = Omit<Dtos["EditFlowInboxRequest"], "flow">;
export type InboxView = Readonly<{
  page: InboxPage | undefined;
  writable: boolean;
  editable: boolean;
  busy: boolean;
  retry: boolean;
  feedback: string;
}>;
export type InboxAction =
  | { kind: "page"; offset: number }
  | { kind: "retry" }
  | { kind: "edit"; page: InboxPage; item: InboxItem; change: Change };

export function inboxLane(page: InboxPage, item: InboxItem): readonly string[] {
  return page.next_turn_lanes.find((lane) => lane.includes(item.id)) ?? [];
}

function allows(page: InboxPage, item: InboxItem, change: Change): boolean {
  if (!page.edits_available || !page.items.includes(item)) return false;
  if (change.action === "cancel") return true;
  if (item.held) return false;
  if (change.action === "edit_text") return !item.text_truncated && change.text !== item.text &&
    new TextEncoder().encode(change.text).length <= 64 * 1024;
  if (change.action === "set_delivery") return change.delivery !== item.delivery &&
    (change.delivery === "next_turn" || (!item.separate_turn && page.insertion_available));
  const lane = inboxLane(page, item);
  const index = lane.indexOf(item.id);
  return item.delivery === "next_turn" && index >= 0 &&
    lane.length === change.expected_order.length && lane.every((id, i) => id === change.expected_order[i]) &&
    (change.direction === "earlier" ? index > 0 : index < lane.length - 1);
}

export type PendingInbox = { request: Request; busy: boolean; feedback: string; changes: EventTarget };

export function observeInbox(flow: FlowClient, signal: AbortSignal, active: () => boolean,
  pending: Map<string, PendingInbox>, edits: boolean, changed: () => void) {
  const key = `${flow.address.session_id}/${flow.address.instance_id}`;
  let head: Dtos["HistoryHead"] | undefined;
  let checkpoint: Dtos["FlowCursor"] | undefined;
  let page: InboxPage | undefined;
  let rejected = "";
  let offset = 0;
  let reading = false;
  let dirty = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let failure = "";
  const listener = () => { refresh(); changed(); };
  const attached = new WeakSet<PendingInbox>();
  function attach(entry: PendingInbox): void {
    if (attached.has(entry)) return;
    attached.add(entry);
    entry.changes.addEventListener("change", listener, { signal });
  }
  const prior = pending.get(key);
  if (prior) attach(prior);
  signal.addEventListener("abort", () => clearTimeout(timer), { once: true });

  function fresh(snapshot: InboxPage | undefined): snapshot is InboxPage {
    if (!snapshot || !checkpoint || !sameHistoryHead(head, snapshot.head)) return false;
    const cursor = snapshot.cursor;
    return cursor.journal_id === checkpoint.journal_id && (cursor.sequence > checkpoint.sequence ||
      (cursor.sequence === checkpoint.sequence && cursor.digest === checkpoint.digest));
  }

  function refresh(): void {
    if (!head || signal.aborted) return;
    dirty = true;
    if (reading || timer !== undefined) return;
    timer = setTimeout(() => { timer = undefined; void read(); }, 80);
  }

  async function read(): Promise<void> {
    if (signal.aborted) return;
    dirty = false;
    reading = true;
    const requested = offset;
    try {
      const { result } = await flow.inbox(requested, { signal });
      if (!signal.aborted && requested === offset && fresh(result)) {
        page = result;
        offset = result.offset;
        failure = "";
      }
    } catch (error) {
      if (!signal.aborted) {
        page = undefined;
        failure = error instanceof Error ? error.message : "Could not read the inbox.";
      }
    } finally {
      reading = false;
      if (!signal.aborted) {
        if (dirty) refresh();
        changed();
      }
    }
  }

  async function issue(entry: PendingInbox): Promise<void> {
    entry.busy = true;
    entry.feedback = "";
    rejected = "";
    changed();
    try {
      const { result } = await flow.editInbox(entry.request);
      if (pending.get(key) === entry) pending.delete(key);
      if (!signal.aborted) {
        invalidate(result.receipt);
        // Retain geometry while the receipt invalidates actions from the old page.
      }
    } catch (error) {
      if ((error instanceof ClientError && error.outcome === "not_sent") ||
        (error instanceof RemoteError && [-32601, -32602, -32002].includes(error.code))) {
        if (pending.get(key) === entry) pending.delete(key);
        rejected = error.message;
        refresh();
      } else {
        entry.feedback = error instanceof RemoteError ? `${error.message} · Queue change unconfirmed.` : "Queue change unconfirmed.";
      }
    } finally {
      entry.busy = false;
      entry.changes.dispatchEvent(new Event("change"));
    }
  }

  function invalidate(cursor: Dtos["FlowCursor"]): void {
    if (!checkpoint || (cursor.journal_id === checkpoint.journal_id && cursor.sequence > checkpoint.sequence))
      checkpoint = cursor;
    refresh();
  }

  return {
    install(next: Dtos["HistoryHead"], cursor: Dtos["FlowCursor"]): void {
      if (!sameHistoryHead(head, next) || checkpoint?.journal_id !== cursor.journal_id) {
        page = undefined;
        rejected = "";
        offset = 0;
        checkpoint = cursor;
        head = next;
        refresh();
      } else if (cursor.sequence > checkpoint.sequence) invalidate(cursor);
    },
    invalidate,
    snapshot(): InboxView | undefined {
      const entry = pending.get(key);
      const visible = page && sameHistoryHead(head, page.head) ? page : undefined;
      if (!visible?.total && !entry && !failure && !rejected) return;
      return { page: visible, editable: edits, busy: entry?.busy === true,
        retry: edits && entry !== undefined && !entry.busy,
        feedback: entry?.feedback || failure || rejected,
        writable: edits && !entry && fresh(visible) && visible.offset === offset && visible.edits_available };
    },
    act(action: InboxAction): void {
      if (!active() || signal.aborted) return;
      if (action.kind === "page") {
        offset = Math.max(action.offset, 0);
        refresh();
        changed();
        return;
      }
      if (!edits) return;
      const entry = pending.get(key);
      if (action.kind === "retry") {
        if (entry && !entry.busy) void issue(entry);
        return;
      }
      if (entry || action.page !== page || !fresh(page) || page.offset !== offset ||
        !allows(page, action.item, action.change)) return;
      const next: PendingInbox = { busy: false, feedback: "", changes: new EventTarget(), request: {
        request_id: crypto.randomUUID(), message_id: action.item.id, expected_head: page.head,
        expected_revision: action.item.revision, change: action.change,
      } };
      pending.set(key, next);
      attach(next);
      void issue(next);
    },
  };
}
