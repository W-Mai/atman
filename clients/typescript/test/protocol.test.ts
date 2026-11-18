import { expect, test } from "bun:test";
import {
  ClientError, cursorId, defineMethod, EventCursor, HttpClient, parseSse,
} from "../src/index.ts";
import type { DurableBatch, DurableCursor, Fetch, FlowSource } from "../src/index.ts";

const flow: FlowSource = {
  session_id: "11111111-1111-1111-1111-111111111111",
  instance_id: "22222222-2222-2222-2222-222222222222",
};
const position = (sequence: number, digest = "a"): DurableCursor => ({
  journal_id: flow.instance_id, sequence, digest: digest.repeat(64),
});
const catalog = () => ({
  protocol_revision: 1, schema_revision: 1, host_generation: "fixture-host",
  methods: {
    "ping": { revision: 1, response_schema: "PingResponse" },
    "flow.invoke": { revision: 2, request_schema: "InvokeFlowRequest", response_schema: "InvokeFlowResponse" },
    "flow.events": { revision: 1, request_schema: "ReadFlowEventsRequest", response_schema: "FlowEventPage" },
  },
});
type Request = { id: string; method: string; params: unknown };
function request(init?: RequestInit): Request {
  return JSON.parse(String(init?.body)) as Request;
}
function response(input: Request, result: unknown): Response {
  return Response.json({ jsonrpc: "2.0", id: input.id, result });
}
function client(fetch: Fetch, timeout = 1000): HttpClient {
  return new HttpClient({
    endpoint: "http://127.0.0.1:1234/api", token: "test-credential",
    contract: { protocol_revision: 1, schema_revision: 1 }, fetch, requestTimeoutMs: timeout,
  });
}
const invoke = defineMethod<{ request_id: string; flow: FlowSource }, unknown>({
  name: "flow.invoke", revision: 2, operation: "command",
  request_schema: "InvokeFlowRequest", response_schema: "InvokeFlowResponse",
  decode: (value) => value,
});

test("command transport failures retain unknown outcome and the original authored request", async () => {
  let sent = 0;
  const authored = Object.freeze({ request_id: "33333333-3333-3333-3333-333333333333", flow });
  const transport = client(async (url, init) => {
    expect(String(url)).toBe("http://127.0.0.1:1234/api/rpc");
    expect(String(url)).not.toContain("test-credential");
    expect(new Headers(init?.headers).get("authorization")).toBe("Bearer test-credential");
    expect(init?.redirect).toBe("error");
    expect(init?.credentials).toBe("omit");
    expect(init?.referrerPolicy).toBe("no-referrer");
    const rpc = request(init);
    if (rpc.method === "capabilities") return response(rpc, catalog());
    sent += 1;
    expect(rpc.params).toEqual(authored);
    return Response.json({ jsonrpc: "2.0", id: "another-request", result: {} });
  });
  await expect(transport.request(invoke, authored)).rejects.toMatchObject({ kind: "protocol", outcome: "unknown" });
  expect(sent).toBe(1);
  expect(authored.request_id).toBe("33333333-3333-3333-3333-333333333333");
  expect(JSON.stringify(transport)).not.toContain("test-credential");

  const timed = client(async (_url, init) => {
    const rpc = request(init);
    if (rpc.method === "capabilities") return response(rpc, catalog());
    return new Promise<Response>((_resolve, reject) => {
      init?.signal?.addEventListener("abort", () => reject(new DOMException("cancelled", "AbortError")), { once: true });
    });
  }, 15);
  await expect(timed.request(invoke, authored)).rejects.toMatchObject({ kind: "timeout", outcome: "unknown" });
});

test("capability mismatches and pre-dispatch cancellation never send a command", async () => {
  for (const mismatch of ["revision", "schema", "missing"] as const) {
    let sent = 0;
    const transport = client(async (_url, init) => {
      const rpc = request(init);
      if (rpc.method !== "capabilities") { sent += 1; throw new Error("unexpected dispatch"); }
      const capabilities = catalog();
      if (mismatch === "revision") capabilities.methods["flow.invoke"].revision = 3;
      if (mismatch === "schema") capabilities.methods["flow.invoke"].response_schema = "OtherResponse";
      if (mismatch === "missing") Reflect.deleteProperty(capabilities.methods, "flow.invoke");
      return response(rpc, capabilities);
    });
    await expect(transport.request(invoke, { request_id: "request", flow })).rejects.toMatchObject({ kind: "capability", outcome: "not_sent" });
    expect(sent).toBe(0);
  }
  let requests = 0;
  const transport = client(async () => { requests += 1; throw new Error("unexpected fetch"); });
  const stop = new AbortController();
  stop.abort();
  await expect(transport.request(invoke, { request_id: "request", flow }, { signal: stop.signal })).rejects.toMatchObject({ outcome: "not_sent" });
  expect(requests).toBe(0);
});

test("asynchronous capability negotiation cannot change captured parameters or descriptor identity", async () => {
  let release!: () => void;
  const ready = new Promise<void>((resolve) => { release = resolve; });
  const original = { request_id: "original-request", flow: { ...flow } };
  const mutable = { ...invoke };
  let sent = 0;
  const transport = client(async (_url, init) => {
    const rpc = request(init);
    if (rpc.method === "capabilities") { await ready; return response(rpc, catalog()); }
    sent += 1;
    expect(rpc.method).toBe("flow.invoke");
    expect(rpc.params).toEqual({ request_id: "original-request", flow });
    return response(rpc, {});
  });
  const pending = transport.request(mutable, original);
  original.request_id = "changed-request";
  original.flow.instance_id = flow.session_id;
  mutable.name = "flow.control";
  release();
  await pending;
  expect(sent).toBe(1);

  const source = new AbortController();
  const replacement = new AbortController();
  const options = { signal: source.signal };
  const cancelled = transport.request(invoke, { request_id: "cancelled", flow }, options);
  options.signal = replacement.signal;
  source.abort();
  await expect(cancelled).rejects.toMatchObject({ kind: "aborted", outcome: "not_sent" });
  expect(sent).toBe(1);
});

function chunks(parts: Uint8Array[]): ReadableStream<Uint8Array> {
  return new ReadableStream({
    start(controller) {
      for (const part of parts) if (part.length !== 0) controller.enqueue(part);
      controller.close();
    },
  });
}

test("SSE decoding preserves UTF-8 and framing across every byte boundary without committing EOF tails", async () => {
  const bytes = new TextEncoder().encode(
    "\uFEFF: keepalive\r\nid: first\r\nevent: flow_update\r\ndata: 中😀\r\ndata: second\r\n\r\n"
    + "id: ignored\0value\ndata:\n\n"
    + "event: flow_update\ndata: incomplete",
  );
  for (let split = 0; split <= bytes.length; split += 1) {
    const events = [];
    for await (const event of parseSse(chunks([bytes.slice(0, split), bytes.slice(split)]))) events.push(event);
    expect(events).toEqual([
      { event: "flow_update", data: "中😀\nsecond", id: "first" },
      { event: "message", data: "", id: "first" },
    ]);
  }
  const crEvents = [];
  for await (const event of parseSse(chunks([new TextEncoder().encode("data: complete\r\r")]))) crEvents.push(event);
  expect(crEvents).toEqual([{ event: "message", data: "complete", id: "" }]);
  const oversized = parseSse(chunks([new TextEncoder().encode(`data: ${"x".repeat(128)}`)]), { maxEventBytes: 64 });
  await expect(oversized.next()).rejects.toMatchObject({ kind: "limit" });
});

interface Batch extends DurableBatch { readonly facts: readonly unknown[] }
function decodeBatch(value: unknown): Batch {
  if (value === null || typeof value !== "object" || !Array.isArray((value as Batch).facts)) throw new Error("invalid batch");
  return value as Batch;
}
const eventsContract = {
  name: "flow.events", revision: 1,
  request_schema: "ReadFlowEventsRequest", response_schema: "FlowEventPage", decodeBatch,
};
function frame(receipt: DurableCursor): string {
  return `event: flow_update\nid: ${cursorId(receipt)}\ndata: ${JSON.stringify({ receipt, facts: ["fact"] })}\n\n`;
}

test("durable observations advance only at delivery, reject conflicts, and close without controlling execution", async () => {
  let cancellations = 0;
  let connections = 0;
  let rpcCommands = 0;
  const transport = client(async (url, init) => {
    if (init?.method === "POST") {
      const rpc = request(init);
      if (rpc.method !== "capabilities") rpcCommands += 1;
      return response(rpc, catalog());
    }
    connections += 1;
    const endpoint = new URL(String(url));
    expect(endpoint.pathname).toBe("/api/events");
    expect(endpoint.searchParams.get("token")).toBeNull();
    expect(endpoint.searchParams.get("after")).toBe(cursorId(position(1)));
    const text = frame(position(1)) + frame(position(2, "b"))
      + frame(connections === 1 ? position(2, "c") : position(3, "d"));
    return new Response(new ReadableStream<Uint8Array>({
      start(controller) { controller.enqueue(new TextEncoder().encode(text)); },
      cancel() { cancellations += 1; },
    }), { headers: { "content-type": "Text/Event-Stream; charset=utf-8" } });
  });
  const first = await transport.subscribeFlowEvents(eventsContract, { flow, after: position(1) });
  expect(first.last?.sequence).toBe(1);
  expect((await first.next()).value?.receipt.sequence).toBe(2);
  expect(first.last?.sequence).toBe(2);
  await expect(first.next()).rejects.toMatchObject({ kind: "source" });
  expect(first.last?.digest).toBe("b".repeat(64));
  expect(cancellations).toBe(1);

  const second = await transport.subscribeFlowEvents(eventsContract, { flow, after: position(1) });
  expect((await second.next()).value?.receipt.sequence).toBe(2);
  await second.close();
  expect(second.last?.sequence).toBe(2);
  const unopened = await transport.subscribeFlowEvents(eventsContract, { flow, after: position(1) });
  await unopened.close();
  expect(unopened.last?.sequence).toBe(1);
  expect(cancellations).toBe(3);
  expect(rpcCommands).toBe(0);
});

test("receipt validation rejects foreign and inexact positions without changing its retained source", () => {
  const cursor = new EventCursor(flow, position(1));
  expect(() => cursor.deliver({ ...position(2), journal_id: flow.session_id })).toThrow(ClientError);
  expect(() => cursor.deliver({ ...position(2), sequence: Number.MAX_SAFE_INTEGER + 1 })).toThrow(ClientError);
  expect(cursor.last?.sequence).toBe(1);
  expect(cursor.deliver(position(1))).toBe(false);
  expect(() => cursor.deliver(position(1, "b"))).toThrow(ClientError);
  expect(cursor.last?.digest).toBe("a".repeat(64));
  expect(Object.isFrozen(cursor.last)).toBe(true);
});

test("cancelling an unread or pending observer releases its response without advancing a cursor", async () => {
  let cancellations = 0;
  const transport = client(async (_url, init) => {
    if (init?.method === "POST") return response(request(init), catalog());
    return new Response(new ReadableStream<Uint8Array>({
      cancel() { cancellations += 1; },
    }), { headers: { "content-type": "text/event-stream" } });
  });
  const stop = new AbortController();
  const unread = await transport.subscribeFlowEvents(eventsContract, { flow }, { signal: stop.signal });
  stop.abort();
  await Promise.resolve();
  expect(cancellations).toBe(1);
  expect(unread.last).toBeUndefined();
  await unread.close();

  const pending = await transport.subscribeFlowEvents(eventsContract, { flow });
  const next = pending.next().catch((error: unknown) => error);
  await pending.close();
  expect(await next).toBeInstanceOf(DOMException);
  expect(cancellations).toBe(2);
  expect(pending.last).toBeUndefined();
});
