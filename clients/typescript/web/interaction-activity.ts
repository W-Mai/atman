import { ClientError, decodeDto, type Dtos, type FlowClient } from "../src/index.ts";
import { sameHistoryHead } from "./history-controller.ts";

type Flow = Pick<FlowClient, "address" | "interactions" | "replyInteraction" | "interactionStatus" | "compactResult">;
type Operation = Dtos["FlowInteractionOperation"];
type Reply = Dtos["FlowInteractionReply"];
type Outcome = Dtos["FlowInteractionOutcome"]["state"] | "not_sent";
type Snapshot = Dtos["ReadFlowInteractionsResponse"];
type CompactResult = Dtos["FlowCompactResult"];
export type InteractionTarget = Readonly<{ flow: Flow; label: string }>;
export type InteractionKind = Operation["kind"];

export type InteractionView = Readonly<{
  key: string;
  label: string;
  snapshot: Snapshot | undefined;
  attention: boolean;
  error: boolean;
  writable: boolean;
  attempt: Readonly<{ state: Outcome; busy: boolean }> | undefined;
  notice: Outcome | undefined;
  compactResult: CompactResult | undefined;
}>;

type Attempt = { operation: Operation; state: Outcome; busy: boolean };
type CompactObservation = { operation: Operation; result?: CompactResult };
const archiveKey = "atman.web.interaction-receipts.v1";
const maxAttempts = 32;
const flowKey = (flow: Flow["address"]) => `${flow.session_id}/${flow.instance_id}`;

export class InteractionActivity {
  #connection: AbortSignal | undefined;
  #scope: string | undefined;
  #command = false;
  #canReadCompact = false;
  #target: InteractionTarget | undefined;
  #head: Dtos["HistoryHead"] | undefined;
  #snapshot: Snapshot | undefined;
  #read: AbortController | undefined;
  #compactRead: AbortController | undefined;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #dirty = false;
  #error = false;
  #view: InteractionView | undefined;
  #attempts = new Map<string, Attempt>();
  #compact = new Map<string, CompactObservation>();
  #notices = new WeakMap<InteractionTarget, { head: Operation["head"]; outcome: Outcome }>();

  constructor(private readonly present: (view: InteractionView | undefined) => void,
    private readonly storage?: Pick<Storage, "getItem" | "setItem" | "removeItem">) {}

  connect(scope: string, signal: AbortSignal, command: boolean, compactResult: boolean): void {
    this.disconnect();
    this.#connection = signal;
    this.#scope = scope;
    this.#command = command;
    this.#canReadCompact = compactResult;
    try {
      const raw = this.storage?.getItem(archiveKey);
      if (!raw || raw.length > 32768) return;
      const value = JSON.parse(raw);
      if (value.scope !== scope || !Array.isArray(value.operations)) return;
      for (const entry of value.operations.slice(0, maxAttempts)) {
        try {
          const operation = decodeDto("FlowInteractionOperation", entry);
          this.#attempts.set(flowKey(operation.flow), { operation, state: "unknown", busy: false });
        } catch { /* Ignore malformed stored identities. */ }
      }
      if (compactResult && Array.isArray(value.compact)) {
        for (const entry of value.compact.slice(0, maxAttempts)) {
          try {
            const operation = decodeDto("FlowInteractionOperation", entry);
            if (operation.kind === "compact_review") this.#compact.set(flowKey(operation.flow), { operation });
          } catch { /* Ignore malformed stored identities. */ }
        }
      }
    } catch { /* Storage is optional. */ }
  }

  disconnect(forget = false): void {
    this.#connection = undefined;
    this.#scope = undefined;
    this.#attempts.clear();
    this.#compact.clear();
    this.#notices = new WeakMap();
    this.select(undefined, undefined);
    if (forget) try { this.storage?.removeItem(archiveKey); } catch { /* Storage is optional. */ }
  }

  select(target: InteractionTarget | undefined, head: Dtos["HistoryHead"] | undefined): void {
    if (!this.#connection || !head) target = undefined;
    if (!target) head = undefined;
    if (target === this.#target && (head === this.#head || (head && sameHistoryHead(this.#head, head)))) return;
    const changedFlow = target !== this.#target;
    this.#read?.abort();
    this.#read = undefined;
    this.#compactRead?.abort();
    this.#compactRead = undefined;
    clearTimeout(this.#timer);
    this.#dirty = false;
    this.#target = target;
    this.#head = head;
    this.#snapshot = undefined;
    this.#error = false;
    this.#show();
    if (target) {
      this.refresh();
      if (changedFlow) {
        const attempt = this.#attempts.get(flowKey(target.flow.address));
        if (attempt && !attempt.busy) void this.#dispatch(target, attempt);
      }
    }
  }

  refresh(): void {
    const target = this.#target;
    const head = this.#head;
    if (!target || !head || this.#connection?.aborted) return;
    const compact = this.#currentCompact();
    if (compact && (!compact.result || compact.result.state === "applying")) void this.#readCompact(target, compact);
    clearTimeout(this.#timer);
    if (this.#read) { this.#dirty = true; return; }
    this.#dirty = false;
    void this.#inspect(target, head);
  }

  invalidate(flow: Flow): void {
    if (flow !== this.#target?.flow || this.#dirty) return;
    this.#dirty = true;
    if (this.#read) return;
    clearTimeout(this.#timer);
    this.#timer = setTimeout(() => this.refresh(), 160);
  }

  issue(view: InteractionView, id: string, kind: InteractionKind, reply: Reply): void {
    const target = this.#target;
    const snapshot = view.snapshot;
    if (view !== this.#view || !target || !view.writable || snapshot?.state !== "ready") return;
    const found = kind === "compact_review" ? snapshot.compact?.id === id
      : snapshot.forms.some((form) => form.id === id && `${form.timing}_form` === kind);
    if (!found) return;
    if (this.#attempts.size >= maxAttempts) {
      this.#notices.set(target, { head: snapshot.head, outcome: "not_sent" });
      this.#show();
      return;
    }
    const operation: Operation = {
      flow: target.flow.address, head: snapshot.head, host_generation: snapshot.host_generation,
      interaction_id: id, request_id: crypto.randomUUID(), kind,
    };
    const attempt: Attempt = { operation, state: "pending", busy: false };
    this.#attempts.set(flowKey(operation.flow), attempt);
    if (kind === "compact_review") {
      this.#compact.delete(flowKey(operation.flow));
      this.#compactRead?.abort();
      this.#compactRead = undefined;
    }
    this.#notices.delete(target);
    this.#save();
    void this.#dispatch(target, attempt, reply);
  }

  check(view: InteractionView): void {
    const target = this.#target;
    if (view !== this.#view || !target) return;
    const attempt = this.#attempts.get(flowKey(target.flow.address));
    if (attempt && !attempt.busy) void this.#dispatch(target, attempt);
    const compact = this.#currentCompact();
    if (compact?.result?.state === "unconfirmed") void this.#readCompact(target, compact);
  }

  dismissUnknown(view: InteractionView): void {
    const target = this.#target;
    if (view !== this.#view || !target || view.attempt?.busy || view.attempt?.state !== "unknown") return;
    this.#attempts.delete(flowKey(target.flow.address));
    this.#save();
    this.#snapshot = undefined;
    this.#read?.abort();
    this.#read = undefined;
    this.refresh();
    this.#show();
  }

  async #inspect(target: InteractionTarget, head: Dtos["HistoryHead"]): Promise<void> {
    const connection = this.#connection;
    if (!connection) return;
    const read = this.#read = new AbortController();
    const signal = AbortSignal.any([read.signal, connection]);
    try {
      const { result } = await target.flow.interactions(head, { signal });
      if (signal.aborted) return;
      this.#snapshot = result;
      this.#error = false;
    } catch {
      if (signal.aborted) return;
      this.#error = true;
    } finally {
      if (this.#read === read) {
        this.#read = undefined;
        const delay = this.#dirty ? 160 : 4000;
        this.#dirty = false;
        this.#timer = setTimeout(() => {
          this.refresh();
          const attempt = this.#attempts.get(flowKey(target.flow.address));
          if (attempt?.state === "pending" && !attempt.busy) void this.#dispatch(target, attempt);
        }, delay);
        this.#show();
      }
    }
  }

  async #dispatch(target: InteractionTarget, attempt: Attempt, reply?: Reply): Promise<void> {
    const signal = this.#connection;
    if (!signal || signal.aborted) return;
    attempt.busy = true;
    this.#show();
    const { flow } = target;
    const { flow: address, ...operation } = attempt.operation;
    const key = flowKey(address);
    try {
      const { result } = await (reply
        ? flow.replyInteraction(operation, reply, { signal })
        : flow.interactionStatus(operation, { signal }));
      if (signal.aborted || this.#connection !== signal) return;
      attempt.state = result.outcome.state;
    } catch (error) {
      if (signal.aborted || this.#connection !== signal) return;
      attempt.state = reply && error instanceof ClientError && error.outcome === "not_sent" ? "not_sent" : "unknown";
    }
    attempt.busy = false;
    if (attempt.state !== "pending" && attempt.state !== "unknown") {
      this.#attempts.delete(key);
      this.#notices.set(this.#target && flowKey(this.#target.flow.address) === key ? this.#target : target,
        { head: operation.head, outcome: attempt.state });
      if (attempt.state === "decided" && this.#canReadCompact) {
        this.#compact.delete(key);
        if (this.#compact.size >= maxAttempts) {
          const oldest = this.#compact.keys().next().value;
          if (oldest !== undefined) this.#compact.delete(oldest);
        }
        this.#compact.set(key, { operation: attempt.operation, result: { state: "applying" } });
      }
      this.#save();
    }
    if (this.#target && flowKey(this.#target.flow.address) === key) {
      if (attempt.state !== "pending" && attempt.state !== "unknown" && attempt.state !== "not_sent") {
        // Retire controls from the last snapshot until the original owner is observed again.
        if (this.#snapshot?.state === "ready" && attempt.state !== "invalid_answer") {
          const { compact, ...snapshot } = this.#snapshot;
          this.#snapshot = { ...snapshot, forms: snapshot.forms.filter(({ id }) => id !== operation.interaction_id),
            ...(compact && compact.id !== operation.interaction_id ? { compact } : {}) };
        }
        this.#read?.abort();
        this.#read = undefined;
        this.refresh();
      }
      this.#show();
    }
  }

  #currentCompact(): CompactObservation | undefined {
    if (!this.#target) return;
    const observed = this.#compact.get(flowKey(this.#target.flow.address));
    return observed && sameHistoryHead(this.#head, observed.operation.head) ? observed : undefined;
  }

  async #readCompact(target: InteractionTarget, observed: CompactObservation): Promise<void> {
    if (this.#compactRead || !this.#connection || this.#connection.aborted) return;
    const read = this.#compactRead = new AbortController();
    const signal = AbortSignal.any([read.signal, this.#connection]);
    const { flow: _, ...operation } = observed.operation;
    try {
      const { result } = await target.flow.compactResult(operation, { signal });
      if (!signal.aborted) observed.result = result.result;
    } catch {
      if (!signal.aborted) observed.result = { state: "unconfirmed" };
    } finally {
      if (this.#compactRead === read) {
        this.#compactRead = undefined;
        this.#show();
      }
    }
  }

  #save(): void {
    try {
      if (this.#attempts.size === 0 && this.#compact.size === 0) this.storage?.removeItem(archiveKey);
      else this.storage?.setItem(archiveKey, JSON.stringify({
        scope: this.#scope, operations: [...this.#attempts.values()].map(({ operation }) => operation),
        compact: [...this.#compact.values()].map(({ operation }) => operation),
      }));
    } catch { /* Keep identities in memory when storage is unavailable. */ }
  }

  #show(): void {
    const target = this.#target;
    const head = this.#head;
    const snapshot = this.#snapshot;
    if (!target || !head) this.#view = undefined;
    else {
      const key = flowKey(target.flow.address);
      const attempt = this.#attempts.get(key);
      const feedback = this.#notices.get(target);
      const notice = feedback && sameHistoryHead(head, feedback.head) ? feedback.outcome : undefined;
      const compactResult = this.#currentCompact()?.result;
      this.#view = {
        key: `${key}/${head.branch}/${head.version}`, label: target.label, snapshot, error: this.#error,
        attention: Boolean(attempt || compactResult?.state === "applying" ||
          (snapshot?.state === "ready" && (snapshot.forms.length || snapshot.compact || snapshot.omitted))),
        writable: this.#command && attempt === undefined && !this.#error && snapshot?.state === "ready",
        attempt: attempt && { state: attempt.state, busy: attempt.busy }, notice, compactResult,
      };
    }
    this.present(this.#view);
  }
}
