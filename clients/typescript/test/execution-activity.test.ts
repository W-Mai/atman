import { expect, test } from "bun:test";
import { ClientError } from "../src/index.ts";
import type { Dtos } from "../src/index.ts";
import { ExecutionActivity } from "../web/execution-activity.ts";
import type { ActivityView, ExecutionFlow } from "../web/execution-activity.ts";

type Snapshot = Dtos["FlowExecutionResponse"];
type Request = Parameters<ExecutionFlow["control"]>[0];
type Change = Partial<Pick<Snapshot, "allowed_actions" | "control" | "cursor" | "paused" | "phase" | "revision">>;
const uuid = (n: number) => `12345678-1234-1234-1234-${String(n).padStart(12, "0")}`;
const scope = uuid(90);
const started = uuid(91);
const recovered = uuid(92);
const tick = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { resolve, reject, promise };
}

function flow(id: number) {
  const address = { session_id: uuid(1), instance_id: uuid(id) };
  const cursor = (sequence: number) => ({ journal_id: address.instance_id, sequence, digest: "a".repeat(64) });
  let current: Snapshot = { flow: address, cursor: cursor(1), revision: 4, loaded: true,
    paused: false, phase: "running", allowed_actions: ["cancel_turn"] };
  const requests: Request[] = [];
  const reads: (string | undefined)[] = [];
  const writes: ReturnType<typeof deferred<{ flow: typeof address; result: Dtos["ControlFlowResponse"] }>>[] = [];
  const client: ExecutionFlow = {
    address,
    async execution(requestId) {
      reads.push(requestId);
      return { flow: address, result: current };
    },
    async control(request) {
      const write = deferred<{ flow: typeof address; result: Dtos["ControlFlowResponse"] }>();
      requests.push(request);
      writes.push(write);
      return write.promise;
    },
  };
  return {
    client, requests, reads, writes, cursor,
    target: { flow: client, label: `Flow ${id}` },
    observe(next: Change) { current = { ...current, ...next }; },
    ack(index: number) { writes[index]!.resolve({ flow: address, result: {
      request_id: requests[index]!.request_id, receipt: cursor(2),
    } }); },
  };
}

function setup(id = () => started, storage?: Pick<Storage, "getItem" | "setItem">) {
  let view: ActivityView | undefined;
  const activity = new ExecutionActivity((current) => { view = current; }, storage, id);
  return { activity, view: () => view! };
}

test("activity follows exact Flow focus and waits for the original Stop completion", async () => {
  const a = flow(2);
  const b = flow(3);
  const { activity, view } = setup();
  try {
    activity.connect(scope, true);
    activity.select(a.target);
    await tick();
    const original = view();
    activity.select(b.target);
    activity.issue(original, "cancel_turn");
    await tick();
    expect([a.requests.length, b.requests.length]).toEqual([0, 0]);
    activity.issue(view(), "cancel_turn");
    activity.select(a.target);
    b.ack(0);
    await tick();
    expect(view().address).toEqual(a.client.address);
    expect(view().pending).toBeUndefined();
    expect(b.requests).toEqual([{ request_id: started, expected_revision: 4, action: "cancel_turn" }]);
    b.observe({ phase: "interrupted", cursor: b.cursor(3), allowed_actions: ["recover"],
      control: { action: "cancel_turn", accepted: b.cursor(2) } });
    activity.select(b.target);
    await tick();
    expect(view().pending?.state).toBe("accepted");
    expect(view().result).toBeUndefined();
    expect(view().canControl).toBeFalse();
    b.observe({ cursor: b.cursor(4), control: { action: "cancel_turn", accepted: b.cursor(2), applied: b.cursor(4) } });
    activity.refresh();
    await tick();
    expect(view().pending).toBeUndefined();
    expect(view().result).toBe("stopped");
    expect(view().canControl).toBeTrue();
    expect(b.reads.at(-1)).toBe(started);
  } finally { activity.disconnect(); }
});

test("reload observes an unknown request read-only and explicit retry keeps ID and revision", async () => {
  let stored: string | null = null;
  const storage = { getItem: () => stored, setItem: (_key: string, value: string) => { stored = value; } };
  const a = flow(2);
  const first = setup(() => started, storage);
  const next = setup(() => recovered, storage);
  try {
    first.activity.connect(scope, true);
    first.activity.select(a.target);
    await tick();
    first.activity.issue(first.view(), "cancel_turn");
    first.activity.disconnect();
    a.writes[0]!.reject(new ClientError("transport", "Connection lost", "unknown"));
    next.activity.connect(scope, true);
    next.activity.select(a.target);
    await tick();
    expect(a.requests).toHaveLength(1);
    expect(a.reads.at(-1)).toBe(started);
    expect(next.view().pending).toEqual({ action: "cancel_turn", state: "unknown", retry: true });
    next.activity.retry(next.view());
    expect(a.requests[1]).toEqual(a.requests[0]!);
    next.activity.select(undefined);
    a.ack(1);
    await tick();
    expect(next.view()).toBeUndefined();
    a.observe({ phase: "interrupted", revision: 5, cursor: a.cursor(4), allowed_actions: ["recover"],
      control: { action: "cancel_turn", accepted: a.cursor(2), applied: a.cursor(4) } });
    next.activity.select(a.target);
    await tick();
    expect(next.view().result).toBe("stopped");
  } finally { next.activity.disconnect(); }
});

test("Recover uses the current authorized revision and confirms its durable result", async () => {
  const a = flow(2);
  const { activity, view } = setup(() => recovered);
  try {
    a.observe({ phase: "interrupted", revision: 13, allowed_actions: ["recover"], paused: true });
    activity.connect(scope, false);
    activity.select(a.target);
    await tick();
    expect(view().canControl).toBeFalse();
    activity.issue(view(), "recover");
    expect(a.requests).toHaveLength(0);
    activity.connect(scope, true);
    activity.select(a.target);
    await tick();
    activity.issue(view(), "recover");
    expect(a.requests).toEqual([{ request_id: recovered, expected_revision: 13, action: "recover" }]);
    a.observe({ phase: "idle", cursor: a.cursor(2), allowed_actions: [],
      control: { action: "recover", accepted: a.cursor(2), applied: a.cursor(2) } });
    a.ack(0);
    await tick();
    expect(view().result).toBe("recovered");
    expect(view().observed?.paused).toBeTrue();
  } finally { activity.disconnect(); }
});

test("late reads and unknown receipts cannot enable a command on a different execution revision", async () => {
  const a = flow(2);
  const b = flow(3);
  const { activity, view } = setup();
  const stale = deferred<Awaited<ReturnType<ExecutionFlow["execution"]>>>();
  let signal: AbortSignal | undefined;
  const delayed: ExecutionFlow = {
    ...a.client,
    async execution(_id, options) {
      signal = options?.signal;
      return stale.promise;
    },
  };
  try {
    activity.connect(scope, true);
    activity.select({ flow: delayed, label: "Old Flow" });
    activity.select(b.target);
    expect(signal?.aborted).toBeTrue();
    stale.resolve(await a.client.execution());
    await tick();
    expect(view().address).toEqual(b.client.address);
    activity.issue(view(), "cancel_turn");
    b.writes[0]!.reject(new ClientError("transport", "Response lost", "unknown"));
    b.observe({ revision: 5, cursor: b.cursor(2) });
    await tick();
    expect(view().pending).toEqual({ action: "cancel_turn", state: "unknown", retry: false });
    activity.retry(view());
    expect(b.requests).toHaveLength(1);
    expect(b.reads.at(-1)).toBe(started);
  } finally { activity.disconnect(); }
});
