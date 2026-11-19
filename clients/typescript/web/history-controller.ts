import { ClientError, canonicalUuid, cursorId } from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import type { HistoryAction, HistoryView } from "./history-ui.ts";

type Head = Dtos["HistoryHead"];
type Page = Dtos["ConversationPage"];
type Record = Dtos["HistoryOperationRecord"];
type Review = Dtos["HistoryReviewResponse"];
type Branches = Extract<HistoryView, { kind: "branches" }>;
type Mode = "revert" | "restore";
type Operation = { readonly flow: Dtos["FlowAddress"]; readonly requestId: string };
type Pending = {
  readonly operation: Operation;
  readonly mode: Mode;
  review?: Review;
  record?: Record;
  running?: string;
  missing?: boolean;
  error?: string;
  timer?: ReturnType<typeof setTimeout>;
};

export type HistoryBinding = {
  flow: FlowClient;
  signal: AbortSignal;
  page: () => Page | undefined;
  active: () => boolean;
  refresh: () => void;
  recoverable: () => boolean;
  recover: (record: Record, input: string, stillCurrent: () => boolean) => Promise<boolean>;
};

const archive = "atman.web.history.v1";
const maximum = 32;
const keyOf = ({ session_id, instance_id }: FlowClient["address"]) => session_id + "/" + instance_id;
export const sameHistoryHead = (a: Head | undefined, b: Head): boolean =>
  a?.branch === b.branch && a.version === b.version;
const detail = (error: unknown) => error instanceof Error ? error.message : "History request failed";

function restorePending(storage: Storage | undefined, scope: string): Map<string, Pending> {
  const entries = new Map<string, Pending>();
  try {
    const raw = storage?.getItem(archive);
    if (!raw || raw.length > 8192) return entries;
    const value = JSON.parse(raw);
    if (value.scope !== scope || !Array.isArray(value.operations)) return entries;
    for (const row of value.operations.slice(0, maximum)) {
      try {
        if (row.mode !== "revert" && row.mode !== "restore") continue;
        const flow = { session_id: canonicalUuid(row.flow.session_id), instance_id: canonicalUuid(row.flow.instance_id) };
        const operation = { flow, requestId: canonicalUuid(row.requestId) };
        entries.set(keyOf(flow), { operation, mode: row.mode });
      } catch { /* Invalid stored IDs do not provide an original request. */ }
    }
  } catch { /* History remains usable without tab storage. */ }
  return entries;
}

/** Observe an original operation after its transcript or dialog is hidden. */
export class HistoryController {
  #scope = "";
  #connection: AbortSignal | undefined;
  #operations = new Map<string, Pending>();
  #binding: HistoryBinding | undefined;
  #presentation: AbortController | undefined;
  #read: AbortController | undefined;
  #view: HistoryView | undefined;
  #presented: Pending | undefined;
  #openingCheck: Page | undefined;
  #branchesReceipt: string | undefined;
  #trigger: HTMLButtonElement | undefined;
  #recoverEnabled = false;

  constructor(
    private readonly present: (view: HistoryView | undefined, restore?: HTMLButtonElement) => void,
    private readonly storage?: Storage,
  ) {}

  connect(scope: string, signal: AbortSignal, recovery: boolean): void {
    this.disconnect();
    this.#scope = scope;
    this.#connection = signal;
    this.#recoverEnabled = recovery;
    this.#operations = restorePending(this.storage, scope);
  }

  disconnect(): void {
    this.hide(false);
    for (const entry of this.#operations.values()) clearTimeout(entry.timer);
    this.#operations.clear();
    this.#connection = undefined;
  }

  hide(restore = true): void {
    this.#presentation?.abort();
    this.#presentation = undefined;
    this.#read?.abort();
    this.#read = undefined;
    this.#binding = undefined;
    this.#presented = undefined;
    this.#openingCheck = undefined;
    this.#branchesReceipt = undefined;
    const trigger = this.#trigger;
    this.#trigger = undefined;
    this.#show(undefined, restore ? trigger : undefined);
  }

  focusChanged(): void {
    if (this.#binding && !this.#binding.active()) this.hide(false);
  }

  installed(binding: HistoryBinding): void {
    if (this.#binding !== binding || !this.#view) return;
    const view = this.#view;
    const head = binding.page()?.head;
    if (view.kind === "branches" && !sameHistoryHead(head, view.head)) {
      this.hide(false);
    } else if (view.kind === "operation" && view.phase === "review" &&
      !sameHistoryHead(head, view.review.request.expected_head)) {
      this.#show({ kind: "operation", mode: view.mode, phase: "review_failed", message: "Branch changed. Review again." });
    }
    if (this.#presented && !(this.#view?.kind === "operation" && this.#view.phase === "pending"))
      this.#publish(binding.flow, this.#presented);
  }

  branches(binding: HistoryBinding, trigger: HTMLButtonElement): void {
    if (!this.#capture(binding, trigger)) return;
    const entry = this.#entry(binding.flow);
    if (entry) {
      this.#presented = entry;
      this.#publish(binding.flow, entry);
      if (entry.error || !entry.record || entry.record.status.state === "stopping") void this.#write(binding.flow, entry, "check");
    } else {
      this.#loadBranches(binding.page()!.head, 0);
    }
  }

  revert(binding: HistoryBinding, item: Dtos["ConversationItem"], trigger: HTMLButtonElement): void {
    const head = binding.page()?.head;
    if (!head || !binding.flow.canRevert(item, head) || !this.#capture(binding, trigger)) return;
    const entry = this.#entry(binding.flow);
    if (entry && this.#unfinished(entry)) {
      this.#presented = entry;
      this.#publish(binding.flow, entry);
      return;
    }
    this.#review("revert", head, (signal) => binding.flow.historyReviewAt(item, head, {
      branch: crypto.randomUUID(), request_id: crypto.randomUUID(),
    }, { signal }));
  }

  dispatch(action: HistoryAction): void {
    const binding = this.#binding;
    const current = this.#view;
    if (!binding || !current || !binding.active()) return;
    const { flow } = binding;
    if (current.kind === "branches") {
      if (action.kind === "review_restore" && !current.loading && sameHistoryHead(current.head, action.head) &&
        current.branches.some(({ id }) => id === action.branch) && current.head.branch !== action.branch) {
        this.#review("restore", current.head, (signal) => flow.historyReview({
          request_id: crypto.randomUUID(),
          expected_head: current.head,
          target: { kind: "restore", branch: action.branch },
        }, { signal }));
      } else if (action.kind === "reload_branches" && sameHistoryHead(current.head, action.head)) {
        this.#loadBranches(action.head, 0);
      } else if (action.kind === "more_branches" && current.next === action.offset &&
        !current.loading && sameHistoryHead(current.head, action.head)) {
        this.#loadBranches(action.head, action.offset, current);
      }
      return;
    }
    if (action.kind === "begin") {
      if (current.phase !== "review" || current.review.request !== action.request ||
        !sameHistoryHead(binding.page()?.head, action.request.expected_head)) return;
      const key = keyOf(flow.address);
      const existing = this.#entry(flow);
      if (existing && this.#unfinished(existing)) return;
      if (!existing && this.#operations.size >= maximum) {
        const terminal = [...this.#operations].find(([, entry]) => !this.#unfinished(entry));
        if (!terminal) {
          this.#show({ kind: "operation", mode: current.mode, phase: "review_failed", message: "Resolve an earlier history operation before starting another." });
          return;
        }
        this.#operations.delete(terminal[0]);
      }
      const entry: Pending = {
        operation: { flow: flow.address, requestId: action.request.request_id },
        mode: current.mode, review: current.review,
      };
      this.#operations.set(key, entry);
      this.#presented = entry;
      this.#save();
      void this.#write(flow, entry, "begin");
      return;
    }
    const entry = this.#entry(flow);
    if (!entry || !("operation" in current) || !("operation" in action) ||
      action.operation !== entry.operation || current.operation !== entry.operation) return;
    if (action.kind === "recover" && current.phase === "applied" && !current.draftOccupied &&
      sameHistoryHead(current.head, action.head) && current.recoverable.some(({ id }) => id === action.input) &&
      entry.record?.status.state === "applied") {
      this.#recover(binding, entry, action.input);
    } else if (action.kind === "branches" && !this.#unfinished(entry)) {
      this.#loadBranches(binding.page()!.head, 0);
    } else if (action.kind === "check") {
      if (entry.record?.status.state === "applied") binding.refresh();
      else void this.#write(flow, entry, "check");
    } else if (action.kind === "retry_begin" && current.phase === "unknown" && current.retry &&
      entry.missing && entry.review && sameHistoryHead(binding.page()?.head, entry.review.request.expected_head)) {
      void this.#write(flow, entry, "begin");
    } else if ((action.kind === "stop" && current.phase === "preparing") ||
      (action.kind === "apply" && current.phase === "ready") ||
      (action.kind === "abort" && ["preparing", "stopping", "ready"].includes(current.phase))) {
      void this.#write(flow, entry, action.kind);
    }
  }

  #capture(binding: HistoryBinding, trigger: HTMLButtonElement): boolean {
    if (!this.#connection || this.#connection.aborted || binding.signal.aborted || !binding.active() || !binding.page()) return false;
    this.hide(false);
    const presentation = new AbortController();
    this.#presentation = presentation;
    this.#binding = binding;
    this.#trigger = trigger;
    binding.signal.addEventListener("abort", () => {
      if (this.#binding === binding) this.hide(false);
    }, { once: true, signal: presentation.signal });
    return true;
  }

  #entry(flow: FlowClient) {
    return this.#operations.get(keyOf(flow.address));
  }

  #unfinished(entry: Pending): boolean {
    return entry.record?.status.state !== "applied" && entry.record?.status.state !== "aborted";
  }

  #show(view: HistoryView | undefined, restore?: HTMLButtonElement): void {
    this.#view = view;
    this.present(view, restore);
  }

  #save(): void {
    try {
      this.storage?.setItem(archive, JSON.stringify({
        scope: this.#scope,
        operations: [...this.#operations.values()].map(({ operation, mode }) => ({ ...operation, mode })),
      }));
    } catch { /* Keep the identity in this connection when the tab archive is unavailable. */ }
  }

  #loadBranches(head: Head, offset: number, previous?: Branches): void {
    const binding = this.#binding;
    if (!binding || !sameHistoryHead(binding.page()?.head, head)) return;
    this.#read?.abort();
    const read = new AbortController();
    this.#read = read;
    this.#presented = undefined;
    if (!previous) this.#branchesReceipt = undefined;
    const branches = previous?.branches ?? [];
    this.#show({ kind: "branches", head, branches, loading: true });
    void binding.flow.historyBranches({ expected_head: head, offset }, { signal: read.signal }).then(({ result }) => {
      if (read.signal.aborted) return;
      const receipt = cursorId(result.observed);
      if (previous && this.#branchesReceipt !== receipt) {
        this.#show({ kind: "branches", head, branches: [], loading: false,
          error: "Branch list changed. Refresh it before selecting." });
        return;
      }
      this.#branchesReceipt = receipt;
      this.#show({ kind: "branches", head, branches: [...branches, ...result.branches],
        loading: false, next: result.next_offset ?? null });
    }).catch((error: unknown) => {
      if (!read.signal.aborted)
        this.#show({ kind: "branches", head, branches, loading: false, error: detail(error) });
    });
  }

  #review(mode: Mode, head: Head, request: (signal: AbortSignal) => Promise<{ result: Review }>): void {
    const binding = this.#binding;
    if (!binding || !sameHistoryHead(binding.page()?.head, head)) return;
    this.#read?.abort();
    const read = new AbortController();
    this.#read = read;
    this.#show({ kind: "operation", mode, phase: "reviewing" });
    void request(read.signal).then(({ result }) => {
      if (read.signal.aborted) return;
      if (!sameHistoryHead(binding.page()?.head, head)) throw new Error("Branch changed. Review again.");
      this.#show({ kind: "operation", mode, phase: "review", review: result });
    }).catch((error: unknown) => {
      if (!read.signal.aborted) this.#show({ kind: "operation", mode, phase: "review_failed", message: detail(error) });
    });
  }

  #publish(flow: FlowClient, entry: Pending, pending = entry.running): void {
    const binding = this.#binding;
    if (!binding || !binding.active() || this.#presented !== entry ||
      keyOf(binding.flow.address) !== keyOf(flow.address)) return;
    const base = { kind: "operation" as const, mode: entry.mode, operation: entry.operation };
    if (pending) return this.#show({ ...base, phase: "pending", label: pending });
    const status = entry.record?.status;
    if (entry.error || !status) {
      this.#show({ ...base, phase: "unknown", message: entry.error ?? "Check the original operation.",
        retry: entry.missing === true && entry.review !== undefined &&
          sameHistoryHead(binding.page()?.head, entry.review.request.expected_head) });
    } else if (status.state === "preparing") {
      this.#show({ ...base, phase: "preparing", active: entry.record?.request.active != null });
    } else if (status.state === "applied") {
      if (this.#view?.kind === "operation" && "operation" in this.#view &&
        this.#view.operation === entry.operation && this.#view.phase === "applied" &&
        sameHistoryHead(binding.page()?.head, status.head)) {
        const draftOccupied = !binding.recoverable();
        if (this.#view.draftOccupied !== draftOccupied) this.#show({ ...this.#view, draftOccupied });
      } else {
        this.#show({ ...base, phase: "installing" });
        void this.#installedHead(binding, entry, status);
      }
    } else this.#show({ ...base, phase: status.state });
  }

  async #installedHead(binding: HistoryBinding, entry: Pending, status: Extract<Record["status"], { state: "applied" }>): Promise<void> {
    const page = binding.page();
    const check = () => this.#binding === binding && binding.active() &&
      this.#view?.kind === "operation" && "operation" in this.#view &&
      this.#view.operation === entry.operation && this.#view.phase === "installing";
    const base = { kind: "operation" as const, mode: entry.mode, operation: entry.operation };
    if (!page || !sameHistoryHead(page.head, status.head)) {
      if (page && page.head.version >= status.head.version && check())
        this.#show({ ...base, phase: "superseded" });
      return;
    }
    if (this.#openingCheck === page) return;
    this.#openingCheck = page;
    try {
      // The host validates each exact continuation. This is capped so a busy journal cannot hold the modal open.
      if (!await verifyAppliedCursor(binding.flow, status.receipt, page.source.cursor, binding.signal))
        throw new Error("Could not verify the applied branch against the installed transcript.");
      if (!check() || !sameHistoryHead(binding.page()?.head, status.head)) return;
      const target = entry.record?.request.target;
      const selectedInput = target?.kind === "revert" && target.cutoff.kind === "before_user_turn" ? [target.cutoff.input] : [];
      const recoverable = this.#recoverEnabled ? (entry.review?.recoverable_inputs ?? selectedInput) : [];
      this.#show({ ...base, phase: "applied", head: status.head, draftOccupied: !binding.recoverable(),
        recoverable: recoverable.map((id, index) => ({ id, label: index === 0 ? "Selected turn input" : "Additional input " + (index + 1) })) });
    } catch (error) {
      if (check()) this.#show({ ...base, phase: "install_failed", message: detail(error) });
    } finally {
      if (this.#openingCheck === page) this.#openingCheck = undefined;
    }
  }

  #recover(binding: HistoryBinding, entry: Pending, input: string): void {
    this.#publish(binding.flow, entry, "Recovering original input…");
    const presentation = this.#view;
    const current = () => this.#binding === binding && this.#view === presentation && binding.active();
    void binding.recover(entry.record!, input, current).then((restored) => {
      if (!current()) return;
      if (restored) this.hide();
      else this.#publish(binding.flow, entry);
    }).catch((error: unknown) => {
      if (current()) this.#show({
        kind: "operation", mode: entry.mode, operation: entry.operation, phase: "install_failed", message: detail(error),
      });
    });
  }

  async #write(flow: FlowClient, entry: Pending, action: "begin" | "check" | "stop" | "apply" | "abort"): Promise<void> {
    const signal = this.#connection;
    if (!signal || signal.aborted || entry.running) return;
    const id = entry.operation.requestId;
    clearTimeout(entry.timer);
    entry.running = { begin: "Preparing…", check: "Checking operation…", stop: "Checking the reviewed run…",
      apply: "Switching branches…", abort: "Aborting…" }[action];
    delete entry.error;
    this.#publish(flow, entry);
    try {
      const response = action === "begin" ? (await flow.historyBegin(entry.review!.request, { signal })).result
        : action === "check" ? (await flow.historyStatus(id, { signal })).result.operation
        : action === "stop" ? (await flow.historyStop(id, { signal })).result
        : action === "apply" ? (await flow.historyApply(id, { signal })).result
        : (await flow.historyAbort(id, { signal })).result;
      if (signal.aborted) return;
      entry.missing = response == null;
      if (response) entry.record = response;
      else entry.error = "The host has no matching operation.";
    } catch (error) {
      if (signal.aborted) return;
      entry.error = detail(error);
      if (action === "begin" && !entry.record && error instanceof ClientError && error.outcome === "not_sent") {
        this.#operations.delete(keyOf(flow.address));
        this.#save();
        if (this.#binding?.flow === flow) this.#show({
          kind: "operation", mode: entry.mode, phase: "review_failed", message: entry.error,
        });
        return;
      }
    } finally {
      delete entry.running;
    }
    if (signal.aborted) return;
    this.#publish(flow, entry);
    if (entry.record?.status.state === "stopping" && !entry.error)
      entry.timer = setTimeout(() => void this.#write(flow, entry, "check"), 750);
    const binding = this.#binding;
    if (entry.record?.status.state === "applied" && binding && this.#presented === entry &&
      keyOf(binding.flow.address) === keyOf(flow.address)) binding.refresh();
  }
}

export async function verifyAppliedCursor(
  flow: Pick<FlowClient, "events">,
  applied: Dtos["FlowCursor"],
  through: Dtos["FlowCursor"],
  signal: AbortSignal,
): Promise<boolean> {
  if (applied.journal_id !== through.journal_id || applied.sequence > through.sequence) return false;
  let after = applied;
  for (let scan = 0; scan < 4 && after.sequence < through.sequence; scan++) {
    const { result } = await flow.events({ after, limit: 128 }, { signal });
    const batch = result.batches.find(({ receipt }) => receipt.sequence === through.sequence);
    if (batch) return cursorId(batch.receipt) === cursorId(through);
    if (!result.next || !result.has_more) return false;
    after = result.next;
  }
  return cursorId(after) === cursorId(through);
}
