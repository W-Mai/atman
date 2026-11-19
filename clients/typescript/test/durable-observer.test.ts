import { expect, test } from "bun:test";
import { AtmanClient, HttpClient, contract, cursorId, methods } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowSource } from "../src/index.ts";
import { observeDurable } from "../web/durable-observer.ts";
import type { DurableObservationState } from "../web/durable-observer.ts";

const source: FlowSource = {
  session_id: "11111111-1111-4111-8111-111111111111",
  instance_id: "22222222-2222-4222-8222-222222222222",
};
const branch = "33333333-3333-4333-8333-333333333333";
const encoder = new TextEncoder();

function receipt(sequence: number, digit = sequence.toString(16)): DurableCursor {
  return { journal_id: source.instance_id, sequence, digest: digit.repeat(64) };
}

function frame(position: DurableCursor): string {
  return [
    "id: " + cursorId(position), "event: flow_update",
    "data: " + JSON.stringify({ receipt: position, facts: [{ draft: "seen" }] }), "", "",
  ].join("\n");
}

function sse(text: string, cancel?: () => void): Response {
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode(text));
      if (cancel === undefined) controller.close();
    },
    cancel() { cancel?.(); },
  });
  return new Response(body, { headers: { "content-type": "text/event-stream" } });
}

function setup(connect: (after: string | null, call: number) => Response,
  config: { rpcStatus?: number; snapshot?: Dtos["ConversationPage"] } = {}) {
  const opened: (string | null)[] = [];
  const rpc: string[] = [];
  const transport = new HttpClient({
    endpoint: "http://atman.example.test",
    token: "browser-fixture-secret",
    contract,
    fetch: async (input, init) => {
      const url = new URL(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
      expect(new Headers(init?.headers).get("authorization")).toBe("Bearer browser-fixture-secret");
      expect(url.searchParams.has("token")).toBe(false);
      if (url.pathname === "/rpc") {
        const request = JSON.parse(String(init?.body)) as { method: string; id: string };
        rpc.push(request.method);
        if (config.rpcStatus) return new Response(null, { status: config.rpcStatus });
        if (request.method === "flow.conversation" && config.snapshot) {
          return Response.json({ jsonrpc: "2.0", id: request.id, result: { state: "ready", page: config.snapshot } });
        }
        expect(request.method).toBe("capabilities");
        return Response.json({ jsonrpc: "2.0", id: request.id, result: {
          ...contract, host_generation: "test-host-generation",
          methods: {
            "flow.events": methods["flow.events"],
            "flow.conversation": methods["flow.conversation"],
          },
        } });
      }
      expect(url.pathname).toBe("/events");
      const after = url.searchParams.get("after");
      opened.push(after);
      return connect(after, opened.length);
    },
  });
  return { flow: new AtmanClient(transport).flow(source), opened, rpc };
}

function watch(fixture: ReturnType<typeof setup>, signal: AbortSignal) {
  const states: DurableObservationState[] = [];
  const batches: Readonly<DurableCursor>[] = [];
  return {
    states, batches,
    options: {
      source, after: receipt(1), signal,
      open: (after: Readonly<DurableCursor>, signal: AbortSignal) => fixture.flow.observeEvents({ after }, { signal }),
      refresh: async (_signal: AbortSignal): Promise<Dtos["ConversationPage"] | undefined> => undefined,
      onBatch: (batch: Readonly<DurableCursor>) => { batches.push(batch); },
      onState: (state: DurableObservationState) => { states.push(state); },
    },
  };
}

const noSleep = async () => true;

test("does not advance past the last complete SDK delivery on connection EOF", async () => {
  const closed = new AbortController();
  let cancelled = 0;
  const second = receipt(2);
  const third = receipt(3);
  const simulation = setup((_after, call) => {
    if (call === 1) return sse(frame(second) + frame(third).slice(0, -2));
    return sse(frame(second) + frame(third), () => { cancelled++; });
  });
  const { options, states, batches } = watch(simulation, closed.signal);
  await observeDurable({
    ...options,
    onBatch: (batch) => {
      options.onBatch(batch);
      if (batch.sequence === 3) closed.abort();
    },
  }, noSleep);

  expect(simulation.opened).toEqual([cursorId(receipt(1)), cursorId(second)]);
  expect(simulation.rpc).toEqual(["capabilities"]);
  expect(batches).toEqual([second, third]);
  expect(states).toEqual([{ state: "connected" }, { state: "reconnecting" }, { state: "connected" }]);
  expect(cancelled).toBe(1);
});

test("backoff is bounded and owner discovery continues from the same receipt", async () => {
  const closed = new AbortController();
  const delays: number[] = [];
  const next = receipt(2);
  const simulation = setup((_after, call) => call <= 9
    ? new Response(null, { status: call === 1 ? 409 : 503 })
    : sse(frame(next), () => {}));
  const { options, states, batches } = watch(simulation, closed.signal);
  await observeDurable({
    ...options,
    onBatch: (batch) => { options.onBatch(batch); closed.abort(); },
  }, async (delay) => { delays.push(delay); return true; });

  expect(simulation.opened).toEqual(Array.from({ length: 10 }, () => cursorId(receipt(1))));
  expect(simulation.rpc).toEqual(["capabilities"]);
  expect(batches).toEqual([next]);
  expect(states).toEqual([{ state: "reconnecting" }, { state: "connected" }]);
  expect(delays).toHaveLength(9);
  expect(delays[0]).toBeGreaterThanOrEqual(240);
  expect(delays[0]).toBeLessThanOrEqual(300);
  expect(delays[1]).toBeGreaterThanOrEqual(480);
  expect(delays[1]).toBeLessThanOrEqual(600);
  expect(Math.min(...delays.slice(-3))).toBeGreaterThanOrEqual(9600);
  expect(Math.max(...delays)).toBeLessThanOrEqual(12_000);
});

test("an unaccepted cursor can be replaced only by a fresh source-checked snapshot", async () => {
  const closed = new AbortController();
  const installed = receipt(6);
  const next = receipt(7);
  const snapshot: Dtos["ConversationPage"] = {
    flow: { ...source }, source: { flow: { ...source }, branch, cursor: { ...installed } },
    head: { branch, version: 4 }, items: [], trailing_bytes: 0,
  };
  const simulation = setup((_after, call) => call === 1
    ? new Response(null, { status: 400 })
    : sse(frame(next), () => {}), { snapshot });
  const { options, states, batches } = watch(simulation, closed.signal);

  await observeDurable({
    ...options,
    refresh: async (signal) => {
      const result = (await simulation.flow.conversation({}, { signal })).result;
      return result.state === "ready" ? result.page : undefined;
    },
    onBatch: (batch) => { options.onBatch(batch); closed.abort(); },
  }, noSleep);

  expect(simulation.opened).toEqual([cursorId(receipt(1)), cursorId(installed)]);
  expect(simulation.rpc).toEqual(["capabilities", "flow.conversation"]);
  expect(batches).toEqual([next]);
  expect(states).toEqual([{ state: "checking_source" }, { state: "connected" }]);
});

test("conflicting receipts cannot repeatedly replace a checked snapshot or publish a foreign one", async () => {
  const initial = receipt(1);
  const confirmed = receipt(2);
  const candidate = (replacement: FlowSource): Dtos["ConversationPage"] => ({
    flow: { ...replacement }, source: { flow: { ...replacement }, branch, cursor: { ...confirmed } },
    head: { branch, version: 4 }, items: [], trailing_bytes: 0,
  });
  for (const replaced of [source, { ...source, instance_id: source.session_id }]) {
    let refreshed = 0;
    const simulation = setup((_after, call) =>
      sse(frame(receipt(call === 1 ? initial.sequence : confirmed.sequence, "a"))));
    const { options, states, batches } = watch(simulation, new AbortController().signal);
    await observeDurable({
      ...options,
      refresh: async () => { refreshed++; return candidate(replaced); },
    }, noSleep);
    expect(refreshed).toBe(1);
    expect(simulation.opened).toEqual(replaced === source
      ? [cursorId(initial), cursorId(confirmed)] : [cursorId(initial)]);
    expect(batches).toEqual([]);
    expect(states.at(-1)).toEqual({ state: "stopped", reason: "source" });
  }
});

test("an authentication or authorization failure stops automatically", async () => {
  for (const status of [401, 403]) {
    for (const source of ["sse", "negotiation"]) {
      const simulation = setup(() => new Response(null, { status }), source === "negotiation" ? { rpcStatus: status } : {});
      const { options, states, batches } = watch(simulation, new AbortController().signal);
      await observeDurable(options, async () => { throw new Error("Unauthorized phase must stop"); });
      expect(simulation.opened).toHaveLength(source === "negotiation" ? 0 : 1);
      expect(simulation.rpc).toEqual(["capabilities"]);
      expect(batches).toEqual([]);
      expect(states).toEqual([{ state: "stopped", reason: status === 401 ? "authentication" : "authorization" }]);
    }
  }
});

test("retiring a view cancels an in-flight wait and does not open another subscription", async () => {
  const closed = new AbortController();
  const simulation = setup(() => new Response(null, { status: 503 }));
  const { options, batches } = watch(simulation, closed.signal);
  await observeDurable({
    ...options, onState: () => { queueMicrotask(() => closed.abort()); },
  });
  expect(simulation.opened).toHaveLength(1);
  expect(batches).toEqual([]);
});
