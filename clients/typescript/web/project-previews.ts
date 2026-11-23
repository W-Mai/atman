import { ClientError, RemoteError } from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";

type Flow = Pick<FlowClient, "address" | "preview">;
type Snapshot = Dtos["FlowPreviewSnapshot"];
type Outcome = Dtos["FlowPreviewOutcome"];
type Observation = { controller: AbortController; wait: boolean };
type Entry = {
  flow: Flow;
  snapshot: Snapshot | undefined;
  unread: Outcome | undefined;
  attached: boolean;
  baseline: boolean;
  due: number;
  failures: number;
  request: Observation | undefined;
};

const longSlots = 6;
const shortSlots = 2;
const shortSpacing = 300;

function sameFlow(left: Flow, right: Flow): boolean {
  return left.address.session_id === right.address.session_id && left.address.instance_id === right.address.instance_id;
}

function sameSource(a: Snapshot, b: Snapshot): boolean {
  return a.edition.host_generation === b.edition.host_generation &&
    a.edition.worker_incarnation === b.edition.worker_incarnation &&
    a.head.branch === b.head.branch && a.head.version === b.head.version;
}

function sameCompletion(a: Snapshot, b: Snapshot): boolean {
  const left = a.completion, right = b.completion;
  if (left == null || right == null) return left == null && right == null;
  return left.outcome === right.outcome && left.lease.run === right.lease.run &&
    left.lease.turn === right.lease.turn && left.lease.epoch === right.lease.epoch;
}

function canRetry(error: unknown): boolean {
  if (error instanceof RemoteError) return error.code === -32000;
  if (!(error instanceof ClientError)) return false;
  return error.kind === "transport" || error.kind === "timeout" || (error.kind === "http" &&
    (error.status === 408 || error.status === 409 || error.status === 429 || (error.status ?? 0) >= 500));
}

/** Keeps compact owner observations independent of the transcripts created by the workspace. */
export class ProjectPreviews {
  readonly #entries = new Map<string, Entry>();
  #focused: string | undefined;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #long = 0;
  #short = 0;
  #nextShort = 0;

  constructor(readonly changed: (session: string) => void, readonly visible: () => ReadonlySet<string>) {}

  view(session: string) {
    const entry = this.#entries.get(session);
    return { text: entry?.snapshot?.text ?? "", working: entry?.attached === true && entry.snapshot?.active != null,
      unread: entry?.unread };
  }

  target(session: string): Flow | undefined { return this.#entries.get(session)?.flow; }

  keep(flow: Flow): void {
    this.#bind(flow);
    this.wake();
  }

  #bind(flow: Flow): Entry {
    const id = flow.address.session_id;
    let entry = this.#entries.get(id);
    if (entry && sameFlow(entry.flow, flow)) return entry;
    entry?.request?.controller.abort();
    entry = { flow, snapshot: undefined, unread: undefined, attached: false, baseline: true,
      due: 0, failures: 0, request: undefined };
    this.#entries.set(id, entry);
    this.changed(id);
    return entry;
  }

  focus(flow: Flow): void {
    const id = flow.address.session_id;
    const entry = this.#bind(flow);
    this.#focused = id;
    entry.request?.controller.abort();
    entry.due = 0;
    entry.failures = 0;
    entry.baseline = true;
    entry.unread = undefined;
    this.changed(id);
    this.wake();
  }

  unfocus(): void {
    if (this.#focused === undefined) return;
    this.#focused = undefined;
    this.wake();
  }

  remove(session: string): void {
    const entry = this.#entries.get(session);
    if (entry === undefined) return;
    this.#entries.delete(session);
    if (this.#focused === session) this.#focused = undefined;
    entry.request?.controller.abort();
    this.changed(session);
    this.wake();
  }

  close(): void {
    for (const entry of this.#entries.values()) entry.request?.controller.abort();
    this.#entries.clear();
    this.#focused = undefined;
    this.#nextShort = 0;
    clearTimeout(this.#timer);
    this.#timer = undefined;
  }

  wake(): void {
    clearTimeout(this.#timer);
    this.#timer = undefined;
    const visible = this.visible();
    const rank = ([id, entry]: [string, Entry]) => id === this.#focused ? 0 :
      entry.attached && entry.snapshot?.active != null ? 1 : visible.has(id) ? 2 : 3;
    const ordered = [...this.#entries].sort((left, right) => rank(left) - rank(right));
    const priority = new Set(ordered.slice(0, longSlots).map(([, entry]) => entry));
    for (const [, entry] of ordered) {
      if (entry.request?.wait && !priority.has(entry)) entry.request.controller.abort();
    }
    let due = Infinity;
    const now = performance.now();
    // Earliest due first among overflow rows, including collapsed Projects.
    for (const [id, entry] of ordered.sort((a, b) => a[1].due - b[1].due)) {
      if (entry.request) continue;
      const wait = priority.has(entry);
      if (wait ? this.#long >= longSlots : this.#short >= shortSlots) continue;
      const ready = wait ? entry.due : Math.max(entry.due, this.#nextShort);
      if (ready > now) {
        due = Math.min(due, ready);
        continue;
      }
      if (!wait) this.#nextShort = now + shortSpacing;
      void this.#read(id, entry, wait);
    }
    if (due !== Infinity) this.#timer = setTimeout(() => this.wake(), Math.max(1, due - now));
  }

  async #read(id: string, entry: Entry, wait: boolean): Promise<void> {
    const request = { controller: new AbortController(), wait };
    entry.request = request;
    if (wait) this.#long++;
    else this.#short++;
    const { signal } = request.controller;
    const current = () => this.#entries.get(id) === entry && entry.request === request && !signal.aborted;
    const started = performance.now();
    try {
      const known = wait && entry.attached && !entry.baseline ? entry.snapshot?.edition : undefined;
      const { result } = await entry.flow.preview(known === undefined ? {} : { known }, { signal });
      if (!current()) return;
      if (result.state === "unavailable") {
        entry.attached = false;
        entry.baseline = true;
        this.#retry(entry);
        this.changed(id);
        return;
      }
      entry.failures = 0;
      entry.due = Math.max(performance.now(), started + (wait ? 120 : 1_000));
      if (result.state !== "ready") return;
      const previous = entry.snapshot;
      const next = result.snapshot;
      const baseline = entry.baseline || previous === undefined || !sameSource(previous, next);
      if (baseline || next.active != null || next.completion == null) {
        entry.unread = undefined;
      } else if (!baseline && previous && !sameCompletion(previous, next)) {
        entry.unread = next.completion.outcome;
      }
      entry.snapshot = next;
      entry.attached = true;
      entry.baseline = false;
      this.changed(id);
    } catch (error) {
      if (!current()) return;
      entry.attached = false;
      entry.baseline = true;
      if (canRetry(error)) this.#retry(entry);
      else entry.due = Infinity;
      this.changed(id);
    } finally {
      if (entry.request === request) entry.request = undefined;
      if (wait) this.#long--;
      else this.#short--;
      this.wake();
    }
  }

  #retry(entry: Entry): void {
    entry.due = performance.now() + Math.min(12_000, 350 * 2 ** entry.failures);
    entry.failures = Math.min(6, entry.failures + 1);
  }
}
