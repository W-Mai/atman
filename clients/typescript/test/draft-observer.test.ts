import { expect, test } from "bun:test";
import { AtmanClient, HttpClient, contract, methods } from "../src/index.ts";
import type { Dtos, Fetch } from "../src/index.ts";
import { createDraftObserver } from "../web/draft-observer.ts";
import type { DraftObservationState } from "../web/draft-observer.ts";

const uuid = (number: number): string => `00000000-0000-4000-8000-${number.toString(16).padStart(12, "0")}`;
const flow = { session_id: uuid(1), instance_id: uuid(2) };
const head = { branch: uuid(3), version: 7 };
const newHead = { branch: uuid(4), version: 8 };
const host = uuid(5);
const worker = uuid(6);
const successor = uuid(7);

const receipt = (sequence: number): Dtos["FlowCursor"] =>
  ({ journal_id: flow.instance_id, sequence, digest: sequence.toString(16).padStart(64, "0") });
const page = (current = head, cursor = receipt(1)): Dtos["ConversationPage"] => ({
  flow, head: current, source: { flow, branch: current.branch, cursor }, items: [], trailing_bytes: 0,
});
const snapshot = (revision: number, cursor: number, incarnation = worker, selected = head): Dtos["FlowDraftSnapshot"] => ({
  flow, head: selected, host_generation: host, worker_incarnation: incarnation, revision,
  after: receipt(cursor), calls: [], handoffs: [], truncated: false,
});
const draftMethod = methods["flow.drafts"];
const capabilities = {
  ...contract, host_generation: "fixture-gateway", methods: {
    "flow.drafts": {
      revision: draftMethod.revision,
      request_schema: draftMethod.request_schema,
      response_schema: draftMethod.response_schema,
    },
  },
};

type Request = {
  readonly id: string;
  readonly method: string;
  readonly params: Dtos["ReadFlowDraftsRequest"];
};

function fixture(read: (request: Request, signal: AbortSignal | null | undefined) => unknown | Promise<unknown>) {
  const requests: Request[] = [];
  const fetch: Fetch = async (url, init) => {
    expect(String(url)).toBe("http://atman.test/rpc");
    expect(new Headers(init?.headers).get("authorization")).toBe("Bearer fixture");
    const request = JSON.parse(String(init?.body)) as Request;
    if (request.method === "capabilities") {
      return Response.json({ jsonrpc: "2.0", id: request.id, result: capabilities });
    }
    expect(request.method).toBe("flow.drafts");
    requests.push(request);
    const result = await read(request, init?.signal);
    if (result instanceof Response) return result;
    return Response.json({ jsonrpc: "2.0", id: request.id, result });
  };
  const client = new AtmanClient(new HttpClient({ endpoint: "http://atman.test", token: "fixture", contract, fetch })).flow(flow);
  return { requests, client };
}

function until(finished: (stop: () => void) => void): Promise<void> {
  return new Promise(finished);
}

const immediate = async () => true;
const ready = (value: Dtos["FlowDraftSnapshot"]) => ({ state: "ready", snapshot: value });

test("long waits use only draft editions and reset atomically for a new worker", async () => {
  let calls = 0;
  const result = fixture((request) => {
    calls++;
    if (calls === 1) return ready(snapshot(10, 2));
    if (calls === 2) return { state: "unchanged", flow, head, known: request.params.known };
    if (calls === 3) return ready(snapshot(11, 3));
    if (calls === 4) return ready(snapshot(1, 4, successor));
    return new Response(null, { status: 401 });
  });
  const parent = new AbortController();
  const states: DraftObservationState[] = [];
  await until((finish) => createDraftObserver({
    flow: result.client, signal: parent.signal,
    onState(state) {
      states.push(state);
      if (state.state === "stopped") { parent.abort(); finish(); }
    },
  }, immediate).bind(page()));
  expect(result.requests.map((request) => request.params.known?.revision)).toEqual([undefined, 10, 10, 11, 1]);
  expect(states.filter((state) => state.state === "ready").map((state) => [state.snapshot.revision, state.reset]))
    .toEqual([[10, false], [11, false], [1, true]]);
  expect(states.at(-1)).toEqual({ state: "stopped", reason: "authentication" });
});

test("detach backs off with a cap and clears the old edition before reopening", async () => {
  let calls = 0;
  const result = fixture(() => {
    calls++;
    if (calls === 1) return ready(snapshot(10, 2));
    if (calls <= 11) return { state: "unavailable", flow };
    return ready(snapshot(1, 4, successor));
  });
  const parent = new AbortController();
  const states: DraftObservationState[] = [];
  const delays: number[] = [];
  await until((finish) => createDraftObserver({
    flow: result.client, signal: parent.signal,
    onState(state) {
      states.push(state);
      if (state.state === "ready" && state.reset) { parent.abort(); finish(); }
    },
  }, async (ms) => { delays.push(ms); return true; }).bind(page()));
  const backoff = delays.filter((ms) => ms > 75);
  expect(backoff).toHaveLength(10);
  expect(backoff[0]).toBeGreaterThanOrEqual(297);
  expect(backoff[0]).toBeLessThanOrEqual(350);
  expect(Math.min(...backoff.slice(-3))).toBeGreaterThanOrEqual(10_200);
  expect(Math.max(...backoff)).toBeLessThanOrEqual(12_000);
  expect(states.map((state) => state.state)).toEqual(["ready", "detached", "ready"]);
  expect(result.requests.map((request) => request.params.known?.revision))
    .toEqual([undefined, 10, ...Array.from({ length: 10 }, () => undefined)]);
});

test("a verified head replacement aborts the wait and ignores a late old response", async () => {
  let releaseOld: ((reply: unknown) => void) | undefined;
  let oldSignal: AbortSignal | null | undefined;
  let oldStarted!: () => void;
  const first = until((finish) => { oldStarted = finish; });
  const result = fixture((request, signal) => {
    if (request.params.head.branch === head.branch) {
      oldSignal = signal;
      const late = new Promise<unknown>((resolve) => { releaseOld = resolve; });
      oldStarted();
      return late;
    }
    return ready(snapshot(1, 2, successor, newHead));
  });
  const parent = new AbortController();
  const states: DraftObservationState[] = [];
  let readyNew!: () => void;
  const arrived = until((finish) => { readyNew = finish; });
  const observer = createDraftObserver({
    flow: result.client, signal: parent.signal,
    onState(state) {
      states.push(state);
      if (state.state === "ready") { parent.abort(); readyNew(); }
    },
  }, immediate);
  observer.bind(page());
  await first;
  observer.bind(page(head, receipt(2)));
  observer.bind(page(newHead));
  expect(oldSignal?.aborted).toBe(true);
  releaseOld?.(ready(snapshot(10, 2)));
  await arrived;
  expect(states.map((state) => state.state)).toEqual(["head_reset", "ready"]);
  expect(result.requests.map((request) => request.params.head.branch)).toEqual([head.branch, newHead.branch]);
});

test("a new head hint requires an independent verified page", async () => {
  const result = fixture(() => ({ state: "history_changed", flow, head: newHead }));
  const parent = new AbortController();
  const states: DraftObservationState[] = [];
  await until((finish) => createDraftObserver({
    flow: result.client, signal: parent.signal,
    onState(state) { states.push(state); finish(); },
  }, immediate).bind(page()));
  parent.abort();
  expect(result.requests).toHaveLength(1);
  expect(states).toEqual([{ state: "stale_head" }]);
});

test("authorization and conflicting owner history stop without retries", async () => {
  for (const mode of ["forbidden", "regression"] as const) {
    let calls = 0;
    const result = fixture(() => {
      calls++;
      if (mode === "forbidden") return new Response(null, { status: 403 });
      return ready(snapshot(calls === 1 ? 10 : 9, 2));
    });
    const parent = new AbortController();
    const states: DraftObservationState[] = [];
    await until((finish) => createDraftObserver({
      flow: result.client, signal: parent.signal,
      onState(state) {
        states.push(state);
        if (state.state === "stopped") { parent.abort(); finish(); }
      },
    }, immediate).bind(page()));
    expect(states.at(-1)).toEqual({ state: "stopped", reason: mode === "forbidden" ? "authorization" : "source" });
    expect(calls).toBe(mode === "forbidden" ? 1 : 2);
  }
});
