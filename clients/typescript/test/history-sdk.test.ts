import { expect, test } from "bun:test";
import { AtmanClient, contract, HttpClient, methods } from "../src/index.ts";
import type { Dtos } from "../src/index.ts";

const ids = [1, 2, 3, 4, 5, 6, 7, 8, 9].map((id) => `00000000-0000-4000-8000-00000000000${id}`);
const [session, instance, branch, destination, operation, input, turn, run, cancellation] =
  ids as [string, string, string, string, string, string, string, string, string];
const flow = { session_id: session, instance_id: instance };
const head = { branch, version: 3 };
const appliedHead = { branch: destination, version: 4 };
const lease = { flow, turn, run, epoch: 2 };
const receipt = (sequence: number, journal_id = instance, digest = "a") =>
  ({ journal_id, sequence, digest: digest.repeat(64) });
const intent = {
  request_id: operation,
  expected_head: head,
  target: {
    kind: "revert",
    branch: destination,
    observed: { flow, branch, cursor: receipt(10) },
    cutoff: { kind: "before_user_turn", lease, input, receipt: receipt(8) },
  },
} as const;
const reviewed = {
  flow, ...intent, execution_revision: 5, active: lease, goal: null,
  queued: [{ message: input, revision: receipt(9) }],
};
const selection = { kind: "create", branch: destination, parent: { flow, branch, cursor: receipt(6) } } as const;
const review = { request: reviewed, selection, affected_turns: 2, admitted_inputs: [input] };
const accepted = { request: reviewed, selection, accepted: receipt(11) };
const stopped = { ...accepted, cancellation: { request_id: cancellation, receipt: receipt(12) } };
const applied = {
  ...stopped,
  status: { state: "applied", receipt: receipt(15), head: appliedHead } as const,
};
const caps = {
  ...contract, host_generation: "fixture",
  methods: Object.fromEntries(Object.values(methods).map((method) => [method.name, {
    revision: method.revision, request_schema: method.request_schema, response_schema: method.response_schema,
  }])),
};
interface Rpc { id: string; method: string; params: unknown }

function fixture(handle: (request: Rpc) => unknown) {
  const transport = new HttpClient({
    endpoint: "http://127.0.0.1:1234/", token: "fixture-token", contract,
    fetch: async (_url, init) => {
      const request = JSON.parse(String(init?.body)) as Rpc;
      const result = request.method === "capabilities" ? caps : handle(request);
      return Response.json({ jsonrpc: "2.0", id: request.id, result });
    },
  });
  return new AtmanClient(transport).flow(flow);
}

test("history uses the frozen review and one operation across stop, status and apply", async () => {
  const received: Rpc[] = [];
  const client = fixture((request) => {
    received.push(request);
    switch (request.method) {
      case "flow.history_review": return review;
      case "flow.history_begin": return { ...accepted, status: { state: "preparing" } };
      case "flow.history_stop": return { ...stopped, status: { state: "stopping" } };
      case "flow.history_status": return {
        flow, request_id: operation,
        operation: { ...stopped, status: { state: "ready", receipt: receipt(14), completion: receipt(13) } },
      };
      case "flow.history_apply": return applied;
      default: throw new Error("unexpected RPC");
    }
  });
  const { result } = await client.historyReview(intent);
  expect((await client.historyBegin(result.request)).result.status.state).toBe("preparing");
  expect((await client.historyStop(operation)).result.status.state).toBe("stopping");
  expect((await client.historyStatus(operation)).result.operation?.status.state).toBe("ready");
  expect((await client.historyApply(operation)).result.status).toEqual(applied.status);
  expect(received.map((request) => request.params)).toEqual([
    { ...intent, flow }, reviewed,
    { flow, request_id: operation }, { flow, request_id: operation }, { flow, request_id: operation },
  ]);
});

test("foreign and altered history acknowledgements preserve command uncertainty", async () => {
  let sent = 0;
  let reply: unknown = { ...applied, status: { ...applied.status, head: { ...appliedHead, version: 5 } } };
  const client = fixture(() => { sent += 1; return reply; });
  const foreign: Dtos["HistoryOperationRequest"] = {
    ...reviewed, flow: { ...flow, session_id: destination },
  };
  await expect(client.historyBegin(foreign)).rejects.toMatchObject({ outcome: "not_sent" });
  expect(sent).toBe(0);
  await expect(client.historyApply(operation)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
  reply = { ...applied, request: { ...reviewed, queued: [{ message: input, revision: receipt(9, instance, "b") }] } };
  await expect(client.historyBegin(reviewed)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
  reply = { flow, request_id: operation,
    operation: { ...stopped, status: { state: "ready", receipt: receipt(14), completion: receipt(13, destination) } } };
  await expect(client.historyStatus(operation)).rejects.toMatchObject({ kind: "source", outcome: "not_applicable" });
});

test("branch pages and reviewed ancestry retain complete journal boundaries", async () => {
  let reply: unknown = {
    flow, head, observed: receipt(10), total: 2, next_offset: 1,
    branches: [{ id: branch, created: receipt(1) }],
  };
  const client = fixture(() => reply);
  const page = await client.historyBranches({ expected_head: head, limit: 1 });
  expect(page.result.next_offset).toBe(1);
  reply = { ...page.result, next_offset: 2 };
  await expect(client.historyBranches({ expected_head: head, limit: 1 })).rejects.toMatchObject({ kind: "source" });
  reply = { ...review, selection: { ...selection, parent: { ...selection.parent, cursor: receipt(8) } } };
  await expect(client.historyReview(intent)).rejects.toMatchObject({ kind: "source" });
  reply = { flow, request_id: operation, operation: null };
  expect((await client.historyStatus(operation)).result.operation).toBeNull();
});

test("Restore retains a branch identity and Abort needs its durable terminal receipt", async () => {
  const restore = { ...intent, target: { kind: "restore", branch: destination } as const };
  const request = { ...reviewed, ...restore, active: null };
  const selected = { kind: "restore", branch: destination } as const;
  const client = fixture((rpc) => {
    if (rpc.method === "flow.history_review") {
      return { request, selection: selected, admitted_inputs: [] };
    }
    return { ...accepted, request, selection: selected, status: { state: "aborted", receipt: receipt(12) } };
  });
  const observed = await client.historyReview(restore);
  expect(observed.result.selection).toEqual(selected);
  expect((await client.historyAbort(operation)).result.status.state).toBe("aborted");
  await expect(client.historyApply(operation)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
});

function conversationPage(items: readonly Dtos["ConversationItem"][]) {
  return {
    state: "ready", page: {
      flow, head, source: intent.target.observed, items, trailing_bytes: 0, before: null,
    },
  };
}

const userRow: Dtos["ConversationItem"] = {
  id: { commit: 8, fact: 0, item: 0, message: 0 },
  body: { kind: "message", role: "user", turn, parts: [{ kind: "text", text: "Before the turn" }] },
  user_turn: intent.target.cutoff,
  work: { lease, role: "work", publication: null },
};

test("Revert binds a displayed User or inserted row to the original inspected cutoff", async () => {
  const seen: Rpc[] = [];
  const client = fixture((request) => {
    seen.push(request);
    return request.method === "flow.conversation" ? conversationPage([userRow]) : review;
  });
  const { result } = await client.conversation({ include_source_parts: true });
  if (result.state !== "ready") throw new Error("expected page");
  const item = result.page.items[0]!;
  const options = { request_id: operation, branch: destination };
  expect(client.canRevert(item, head)).toBe(true);
  expect(client.canRevert(item, appliedHead)).toBe(false);
  expect(client.canRevert({ ...item }, head)).toBe(false);
  await expect(client.historyReviewAt({ ...item }, head, options)).rejects.toMatchObject({ outcome: "not_sent" });
  await expect(client.historyReviewAt(item, appliedHead, options)).rejects.toMatchObject({ outcome: "not_sent" });
  Object.assign(item, { user_turn: { ...intent.target.cutoff, input: cancellation } });
  expect((await client.historyReviewAt(item, head, options)).result.selection).toEqual(selection);
  expect(seen.map((request) => request.params)).toEqual([
    { flow, limit: 20, include_source_parts: true },
    { flow, ...intent },
  ]);
});

test("only a verified visible User body can become a history action", async () => {
  let response = conversationPage([{ ...userRow, body: { kind: "deferred", required_at_least: 400, allowed: 10 } }]);
  const client = fixture(() => response);
  const { result } = await client.conversation();
  if (result.state !== "ready") throw new Error("expected page");
  expect(client.canRevert(result.page.items[0]!, head)).toBe(false);
  if (userRow.body.kind !== "message") throw new Error("expected inline body");
  response = conversationPage([{ ...userRow, body: { ...userRow.body, turn: cancellation } }]);
  await expect(client.conversation()).rejects.toMatchObject({ kind: "source" });
});

test("input recovery uses the applied operation and restores the authored payload", async () => {
  const original = {
    flow, request_id: operation, input, head: appliedHead,
    acceptance: receipt(7), recoverable_inputs: [input],
    text: "user draft", quote: "original quote",
    images: [{ data_base64: "aGVsbG8=", name: null }],
  };
  let reply: unknown = original;
  const seen: Rpc[] = [];
  const client = fixture((request) => { seen.push(request); return reply; });
  const { result } = await client.historyRecoverInput(applied, input);
  expect(result).toEqual(original);
  expect(seen[0]?.params).toEqual({ flow, request_id: operation, input, applied_head: appliedHead });
  await expect(client.historyRecoverInput({ ...applied, status: { state: "preparing" } }, input))
    .rejects.toMatchObject({ outcome: "not_sent" });
  expect(seen).toHaveLength(1);
  reply = { ...original, head };
  await expect(client.historyRecoverInput(applied, input)).rejects.toMatchObject({ kind: "source" });
  reply = { ...original, acceptance: receipt(16) };
  await expect(client.historyRecoverInput(applied, input)).rejects.toMatchObject({ kind: "source" });
  reply = { ...original, recoverable_inputs: [cancellation] };
  await expect(client.historyRecoverInput(applied, input)).rejects.toMatchObject({ kind: "source" });
});
