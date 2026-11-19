import { ClientError, canonicalUuid } from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";

export type ExecutionFlow = Pick<FlowClient, "address" | "execution" | "control">;
export type ExecutionAction = "cancel_turn" | "recover";
export type ExecutionTarget = { readonly flow: ExecutionFlow; readonly label: string };
type Observation = Dtos["FlowExecutionResponse"];
type Request = Readonly<{ request_id: string; expected_revision: number; action: ExecutionAction }>;
type RequestState = "sending" | "unknown" | "accepted";
type Pending = { readonly request: Request; state: RequestState; missing: boolean };
type Result = "stopped" | "recovered" | "not_sent";
type StoragePort = Pick<Storage, "getItem" | "setItem">;

export type ActivityView = Readonly<{
  key: string;
  label: string;
  address: ExecutionFlow["address"];
  observed: Observation | undefined;
  canControl: boolean;
  attention: boolean;
  error: string | undefined;
  pending: Readonly<{ action: ExecutionAction; state: RequestState; retry: boolean }> | undefined;
  result: Result | undefined;
}>;

const busy = new Set(["running", "waiting", "finishing", "cancelling", "closing"]);
const maximumPending = 32;
const archive = "atman.web.execution.v1";
const keyOf = (flow: ExecutionFlow): string => `${flow.address.session_id}/${flow.address.instance_id}`;

function load(storage: StoragePort | undefined, scope: string): Map<string, Pending> {
  const pending = new Map<string, Pending>();
  try {
    const raw = storage?.getItem(archive);
    if (!raw || raw.length > 16_384) return pending;
    const value = JSON.parse(raw);
    if (value?.scope !== scope || !Array.isArray(value.requests)) return pending;
    for (const row of value.requests.slice(0, maximumPending)) {
      try {
        if (typeof row?.key !== "string") continue;
        const ids = row.key.split("/");
        if (ids.length !== 2) continue;
        const key = ids.map(canonicalUuid).join("/");
        const request = row?.request;
        if (!Number.isSafeInteger(request?.expected_revision) || request.expected_revision < 0 ||
          (request.action !== "cancel_turn" && request.action !== "recover")) continue;
        pending.set(key, { state: "unknown", missing: false, request: {
          request_id: canonicalUuid(request.request_id), expected_revision: request.expected_revision,
          action: request.action,
        } });
      } catch { /* An invalid row has no authority. */ }
    }
  } catch { /* Browsing with storage disabled still supports a live connection. */ }
  return pending;
}

/** The rail observes one visible Flow. A write keeps its original address and ID across view changes. */
export class ExecutionActivity {
  #target: ExecutionTarget | undefined;
  #scope: string | undefined;
  #live: AbortController | undefined;
  #reading: AbortController | undefined;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #dirty = false;
  #writeEnabled = false;
  #observed: Observation | undefined;
  #error: string | undefined;
  #pending = new Map<string, Pending>();
  #results = new Map<string, Result>();
  #view: ActivityView | undefined;

  constructor(private readonly present: (view: ActivityView | undefined) => void,
    private readonly storage?: StoragePort,
    private readonly id: () => string = () => crypto.randomUUID()) {}

  connect(scope: string, writeEnabled: boolean): void {
    this.disconnect();
    if (scope !== this.#scope) {
      this.#scope = scope;
      this.#pending = load(this.storage, scope);
      this.#results.clear();
    }
    this.#writeEnabled = writeEnabled;
    this.#live = new AbortController();
  }

  disconnect(): void {
    this.#live?.abort();
    this.#live = undefined;
    this.select(undefined);
    for (const pending of this.#pending.values()) {
      if (pending.state === "sending") pending.state = "unknown";
      pending.missing = false;
    }
  }

  select(target: ExecutionTarget | undefined): void {
    if (!this.#live) target = undefined;
    if (target?.flow === this.#target?.flow && target?.label === this.#target?.label) return;
    this.#target = target;
    this.#reading?.abort();
    this.#reading = undefined;
    this.#dirty = false;
    this.#observed = undefined;
    this.#error = undefined;
    this.#clearTimer();
    this.#show();
    this.refresh();
  }

  refresh(flow?: ExecutionFlow): void {
    if (!this.#target || !this.#live || (flow && flow !== this.#target.flow)) return;
    this.#clearTimer();
    if (this.#reading) {
      this.#dirty = true;
      return;
    }
    void this.#inspect(this.#target);
  }

  invalidate(flow: ExecutionFlow): void {
    if (!this.#live || flow !== this.#target?.flow) return;
    if (this.#reading) this.#dirty = true;
    else if (this.#timer === undefined) this.#timer = setTimeout(() => this.refresh(), 200);
  }

  issue(view: ActivityView, action: ExecutionAction): void {
    if (this.#view !== view || !view.canControl || !view.observed?.allowed_actions.includes(action) ||
      !this.#target || this.#pending.size >= maximumPending) return;
    const pending: Pending = { state: "sending", missing: false, request: Object.freeze({
      request_id: this.id(), expected_revision: view.observed.revision, action,
    }) };
    this.#pending.set(view.key, pending);
    this.#results.delete(view.key);
    this.#persist();
    void this.#write(this.#target, pending, false);
  }

  retry(view: ActivityView): void {
    if (this.#view !== view || !view.pending?.retry || !this.#target) return;
    const pending = this.#pending.get(view.key);
    if (!pending) return;
    pending.state = "sending";
    pending.missing = false;
    void this.#write(this.#target, pending, true);
  }

  #clearTimer(): void {
    if (this.#timer !== undefined) clearTimeout(this.#timer);
    this.#timer = undefined;
  }

  #show(): void {
    const target = this.#target;
    const observed = this.#observed;
    if (!target || !this.#live) {
      this.#view = undefined;
    } else {
      const key = keyOf(target.flow);
      const item = this.#pending.get(key);
      this.#view = Object.freeze({
        key, label: target.label, address: target.flow.address, observed, error: this.#error,
        attention: !this.#error && observed !== undefined && busy.has(observed.phase),
        canControl: this.#writeEnabled && !this.#error && !item && observed !== undefined &&
          this.#pending.size < maximumPending,
        pending: item === undefined ? undefined : Object.freeze({
          action: item.request.action, state: item.state,
          retry: this.#writeEnabled && !this.#error && item.state === "unknown" && item.missing &&
            observed !== undefined && observed.revision === item.request.expected_revision &&
            observed.allowed_actions.includes(item.request.action),
        }),
        result: this.#results.get(key),
      });
    }
    this.present(this.#view);
  }

  #persist(): void {
    try {
      this.storage?.setItem(archive, JSON.stringify({ scope: this.#scope,
        requests: [...this.#pending].map(([key, { request }]) => ({ key, request })),
      }));
    } catch { /* The original request ID remains available in memory. */ }
  }

  #result(key: string, value: Result): void {
    this.#results.set(key, value);
    if (this.#results.size > maximumPending) {
      const oldest = this.#results.keys().next().value;
      if (oldest !== undefined) this.#results.delete(oldest);
    }
  }

  async #inspect(target: ExecutionTarget): Promise<void> {
    const read = new AbortController();
    const key = keyOf(target.flow);
    const initial = this.#pending.get(key);
    this.#reading = read;
    try {
      const { result } = await target.flow.execution(initial?.request.request_id, { signal: read.signal });
      if (this.#reading !== read) return;
      const pending = this.#pending.get(key);
      const prior = this.#observed;
      if (prior !== undefined && result.cursor.sequence < prior.cursor.sequence) return;
      if (initial && pending === initial) {
        const receipt = result.control;
        if (receipt != null) {
          if (receipt.action !== pending.request.action) throw new Error("Unrelated control receipt");
          if (receipt.applied != null) {
            this.#pending.delete(key);
            this.#result(key, receipt.action === "cancel_turn" ? "stopped" : "recovered");
            this.#persist();
          } else {
            pending.state = "accepted";
            pending.missing = false;
          }
        } else {
          // An absent record is not a rejection, and never starts a retry.
          pending.missing = pending.state === "unknown";
        }
      }
      this.#observed = result;
      this.#error = undefined;
      this.#show();
    } catch {
      if (this.#reading !== read) return;
      this.#error = "Can’t read execution state.";
      this.#show();
    } finally {
      if (this.#reading === read) {
        this.#reading = undefined;
        if (this.#dirty) {
          this.#dirty = false;
          this.refresh();
        } else if (busy.has(this.#observed?.phase ?? "") || this.#pending.get(key)?.state === "accepted") {
          this.#timer = setTimeout(() => this.refresh(), this.#error ? 2500 : 750);
        }
      }
    }
  }

  async #write(target: ExecutionTarget, pending: Pending, retried: boolean): Promise<void> {
    const live = this.#live;
    if (!live) return;
    const key = keyOf(target.flow);
    const current = () => this.#live === live && this.#pending.get(key) === pending;
    this.#show();
    try {
      await target.flow.control(pending.request, { signal: live.signal });
      if (current()) pending.state = "accepted";
    } catch (error) {
      if (!current() || pending.state === "accepted") return;
      if (!retried && error instanceof ClientError && error.outcome === "not_sent") {
        this.#pending.delete(key);
        this.#result(key, "not_sent");
        this.#persist();
      } else {
        pending.state = "unknown";
      }
    } finally {
      if (this.#live === live && this.#target && keyOf(this.#target.flow) === key) {
        this.#show();
        this.refresh();
      }
    }
  }
}
