import { expect, test } from "bun:test";
import { AtmanClient, contract, cursorId, HttpClient, methods } from "../src/index.ts";
import type { Dtos, DurableCursor, Fetch, FlowSource } from "../src/index.ts";

const flow: FlowSource = {
  session_id: "11111111-1111-1111-1111-111111111111",
  instance_id: "22222222-2222-2222-2222-222222222222",
};
const other = "33333333-3333-3333-3333-333333333333";
const requestId = "44444444-4444-4444-4444-444444444444";
const publication = "55555555-5555-5555-5555-555555555555";
const messageId = "66666666-6666-6666-6666-666666666666";
const receipt = (journal_id = flow.instance_id, sequence = 1, digest = "a") => ({
  journal_id, sequence, digest: digest.repeat(64),
} satisfies DurableCursor);
const catalog = () => ({
  ...contract, host_generation: "fixture-host",
  methods: Object.fromEntries(Object.values(methods).map((method) => [method.name, {
    revision: method.revision, request_schema: method.request_schema, response_schema: method.response_schema,
  }])),
});
interface Rpc { id: string; method: string; params: Record<string, unknown> }
function rpc(init?: RequestInit): Rpc { return JSON.parse(String(init?.body)) as Rpc; }
function response(request: Rpc, result: unknown): Response {
  return Response.json({ jsonrpc: "2.0", id: request.id, result });
}
function api(fetch: Fetch): AtmanClient {
  return new AtmanClient(new HttpClient({ endpoint: "http://127.0.0.1:1234/", token: "fixture-token", contract, fetch }));
}
function fixture(handler: (request: Rpc) => unknown | Promise<unknown>): AtmanClient {
  return api(async (_url, init) => {
    const request = rpc(init);
    return response(request, request.method === methods.capabilities.name ? catalog() : await handler(request));
  });
}
const accepted = () => ({
  flow: { ...flow }, request_id: requestId,
  acceptance: { message_id: messageId, receipt: receipt(), disposition: { state: "pending" } },
});
const sessionView = () => ({
  session_id: flow.session_id, title: "Session", project_root: null,
  default_flow: null, flow_count: 0, catalog: receipt(flow.session_id),
});

test("pending interactions retain the exact Flow, displayed head, and distinct request IDs", async () => {
  const head = { branch: other, version: 4 };
  const form = {
    id: publication, run_id: messageId, timing: "waiting" as const,
    questions: [{ prompt: "Name the branch", input: { kind: "text" as const, multiline: true } }],
  };
  const ready = {
    flow: { ...flow }, head, state: "ready" as const, host_generation: "original-owner", forms: [form], omitted: false,
  };
  let reply: Dtos["ReadFlowInteractionsResponse"] = ready;
  const client = fixture((rpc) => {
    expect(rpc.method).toBe("flow.interactions");
    expect(rpc.params).toEqual({ flow, head });
    return reply;
  }).session(flow.session_id).flow(flow.instance_id);
  expect((await client.interactions(head)).result).toEqual(ready);
  reply = { ...ready, head: { ...head, version: 5 } };
  await expect(client.interactions(head)).rejects.toMatchObject({ kind: "source" });
  reply = { ...ready, flow: { ...flow, instance_id: other } };
  await expect(client.interactions(head)).rejects.toMatchObject({ kind: "source" });
  reply = { ...ready, forms: [form, form] };
  await expect(client.interactions(head)).rejects.toMatchObject({ kind: "source" });
  reply = { flow: { ...flow }, head, state: "source_changed" };
  expect((await client.interactions(head)).result).toEqual(reply);
});

test("interaction reply and status validate the original operation and deferred receipt", async () => {
  const operation = {
    head: { branch: other, version: 4 },
    host_generation: "original-owner",
    interaction_id: publication,
    request_id: requestId,
    kind: "deferred_form" as const,
  };
  const wireOperation = { ...operation, flow: { ...flow } };
  const reply = { kind: "answer_form" as const, answers: [{ kind: "select" as const, index: 0 }] };
  const applied = {
    operation: wireOperation,
    outcome: { state: "deferred" as const, message_id: messageId, accepted: receipt() },
  };
  let response: Dtos["FlowInteractionReplyResponse"] = applied;
  const client = fixture((rpc) => {
    expect(rpc.params).toEqual(rpc.method === "flow.interaction_status" ? wireOperation : { operation: wireOperation, reply });
    return response;
  }).session(flow.session_id).flow(flow.instance_id);
  expect((await client.replyInteraction(operation, reply)).result).toEqual(applied);
  expect((await client.interactionStatus(operation)).result).toEqual(applied);
  response = { ...applied, operation: { ...wireOperation, host_generation: "replacement" } };
  await expect(client.interactionStatus(operation)).rejects.toMatchObject({ kind: "source" });
  response = { ...applied, outcome: { ...applied.outcome, accepted: receipt(other) } };
  await expect(client.interactionStatus(operation)).rejects.toMatchObject({ kind: "source" });
  response = { ...applied, outcome: { state: "resumed" } };
  await expect(client.interactionStatus(operation)).rejects.toMatchObject({ kind: "source" });
});

test("registered artifact pages keep their original part, receipt, and UTF-8 byte boundaries", async () => {
  const locator = {
    source: { flow: { ...flow }, branch: other, cursor: receipt() },
    head: { branch: other, version: 0 },
    item: { commit: 1, fact: 0, item: 0, message: 0 },
    part: 1, id: "out_" + "a".repeat(32),
  };
  const page = {
    state: "ready" as const, locator, content: "🦀", offset: 0, next_offset: 4, total_bytes: 7, has_more: true,
  };
  let reply: Dtos["ReadConversationArtifactResponse"] = page;
  let reads = 0;
  const client = fixture((rpc) => {
    expect(rpc.method).toBe("flow.conversation_artifact");
    reads += 1;
    return reply;
  }).session(flow.session_id).flow(flow.instance_id);
  const request = { locator, offset: 0, limit: 4 };
  expect((await client.conversationArtifact(request)).result).toEqual(page);
  reply = { ...page, locator: { ...locator, part: 0 } };
  await expect(client.conversationArtifact(request)).rejects.toMatchObject({ kind: "source" });
  reply = { ...page, next_offset: 2 };
  await expect(client.conversationArtifact(request)).rejects.toMatchObject({ kind: "source" });
  const foreign = { ...locator, source: { ...locator.source, flow: { ...flow, session_id: other } } };
  await expect(client.conversationArtifact({ ...request, locator: foreign })).rejects.toMatchObject({ kind: "source" });
  const verify = { ...request, verification: { id: requestId } };
  reply = { state: "verifying", locator, verified_bytes: 4, total_bytes: 7 };
  expect((await client.conversationArtifact(verify)).result).toEqual(reply);
  await expect(client.conversationArtifact(request)).rejects.toMatchObject({ kind: "source" });
  reply = { ...reply, locator: { ...locator, part: 0 } };
  await expect(client.conversationArtifact(verify)).rejects.toMatchObject({ kind: "source" });
  reply = { state: "verification_cancelled" };
  await expect(client.conversationArtifact(verify)).rejects.toMatchObject({ kind: "source" });
  expect((await client.conversationArtifact({
    ...verify, verification: { id: requestId, cancel: true },
  })).result).toEqual(reply);
  expect(reads).toBe(8);
});

test("Session targets keep create, open, catalog and canonical bootstrap on their original identities", async () => {
  const calls: Rpc[] = [];
  const client = fixture((request) => {
    calls.push(request);
    expect(request.params.session_id).toBe(flow.session_id);
    switch (request.method) {
      case methods["session.create"].name:
      case methods["session.open"].name: return sessionView();
      case methods["session.flows"].name: return {
        catalog: receipt(flow.session_id), total: 1,
        instances: [{ flow, loaded: false, archived: false }],
      };
      case methods["session.bootstrap"].name: return {
        flow: { session_id: flow.session_id, instance_id: flow.session_id }, receipt: receipt(flow.session_id),
      };
      default: throw new Error("unexpected method");
    }
  });
  const created = await client.createSession({ session_id: flow.session_id, title: "Session" });
  expect(created.session_id).toBe(flow.session_id);
  expect(created.result.flow_count).toBe(0);
  expect((await client.openSession(flow.session_id)).session_id).toBe(flow.session_id);
  const session = client.session(flow.session_id);
  expect((await session.flows({ limit: 1 })).result.instances[0]?.flow).toEqual({ ...flow });
  const boot = await session.bootstrap({ expected_catalog: receipt(flow.session_id) });
  expect(boot.result.flow.instance_id).toBe(flow.session_id);
  expect(session.flow(flow.instance_id).address).toEqual(flow);
  expect(Object.isFrozen(session)).toBe(true);
  expect(calls.map((call) => call.method)).toEqual([
    methods["session.create"].name, methods["session.open"].name,
    methods["session.flows"].name, methods["session.bootstrap"].name,
  ]);
});

test("a Project page remains bound to the revision and offset it continued", async () => {
  const revision = "e".repeat(64);
  const row = { fingerprint: "project", display_name: "Project", project_root: "/workspace",
    pinned: true, archived: false, last_opened: "2026-11-19T00:00:00Z" };
  const cursor = { revision, offset: 1 };
  let result: unknown = { revision, projects: [row], after: cursor };
  const client = fixture(() => result);
  const first = await client.listProjects({ limit: 1 });
  expect(first.projects).toEqual([row]);
  expect(first.after).toEqual(cursor);
  result = { revision: "f".repeat(64), projects: [row], after: null };
  await expect(client.listProjects({ after: cursor })).rejects.toMatchObject({ kind: "source" });
  result = { revision, projects: [row], after: { revision, offset: 5 } };
  await expect(client.listProjects({ after: cursor })).rejects.toMatchObject({ kind: "source" });
});

test("Project Session pages retain the exact scope and distinct native and legacy sources", async () => {
  const revision = "b".repeat(64);
  const cursor = { revision, offset: 2 };
  const target = { kind: "sessions", project_root: "/workspace" } as const;
  const row = { id: flow.session_id, title: "Session", available: true, updated_at: null };
  const rows = { kind: "sessions", project_root: target.project_root, sessions: [
    { ...row, kind: "native" }, { ...row, kind: "legacy" },
  ] } as const;
  const page = { rows, total: 3, revision, after: cursor, sort: "name_ascending", catalog_revision: "c".repeat(64) } as const;
  let result: unknown = { state: "ready", page };
  const client = fixture((call) => {
    expect(call.method).toBe("project.directory");
    expect(call.params.target).toEqual(target);
    expect(call.params.sort).toBe("name_ascending");
    return result;
  });
  const request = { target, sort: "name_ascending", limit: 2 } as const;
  expect(await client.readProjectDirectory(request)).toEqual({ state: "ready", page });
  result = { state: "ready", page: { ...page, rows: { ...rows, project_root: "/other" } } };
  await expect(client.readProjectDirectory(request)).rejects.toMatchObject({ kind: "source" });
  result = { state: "ready", page: { ...page, rows: { ...rows, sessions: [rows.sessions[0], rows.sessions[0]] } } };
  await expect(client.readProjectDirectory(request)).rejects.toMatchObject({ kind: "source" });
  result = { state: "ready", page: { ...page, after: { ...cursor, offset: 3 } } };
  await expect(client.readProjectDirectory(request)).rejects.toMatchObject({ kind: "source" });
  result = { state: "changed" };
  expect(await client.readProjectDirectory({ ...request, after: cursor })).toEqual({ state: "changed" });
  await expect(client.readProjectDirectory(request)).rejects.toMatchObject({ kind: "source" });
});

test("execution observation binds acceptance and completion to the original target and request", async () => {
  const base = { flow, cursor: receipt(flow.instance_id, 3), revision: 2, loaded: true,
    paused: false, phase: "cancelling", allowed_actions: [], request_id: requestId };
  const control = { action: "cancel_turn" as const, accepted: receipt(flow.instance_id, 2), applied: null };
  let result: unknown = { ...base, control };
  const client = fixture((request) => {
    expect(request.method).toBe("flow.execution");
    expect(request.params).toEqual({ flow, request_id: requestId });
    return result;
  }).flow(flow);
  expect((await client.execution(requestId)).result.control).toEqual(control);
  result = { ...base, control: { ...control, applied: base.cursor } };
  expect((await client.execution(requestId)).result.control?.applied).toEqual(base.cursor);
  result = { ...base, request_id: other, control };
  await expect(client.execution(requestId)).rejects.toMatchObject({ kind: "source" });
  result = { ...base, control: { ...control, applied: receipt(flow.instance_id, 3, "b") } };
  await expect(client.execution(requestId)).rejects.toMatchObject({ kind: "source" });
});

test("approval actions require a current review and an exact typed decision receipt", async () => {
  let supportsReceipts = false;
  let decision = { action: "approved", decision_id: publication };
  let writes = 0;
  const client = fixture((request) => {
    if (request.method === "flow.approval_queue") {
      expect(request.params).toEqual({ flow });
      return { state: "ready", flow: { ...flow }, groups: [], ...(supportsReceipts ? { decision_receipts: true } : {}),
        requests: [{ request_id: requestId, requesting_run_id: other, revision: 3, tier: "two", tool: "fs.write",
          detail: { risks: [], targets: [], network: false, incomplete: false } }] } satisfies Dtos["FlowApprovalQueueResponse"];
    }
    expect(request.method).toBe("resolve_permission_requests");
    writes++;
    expect(request.params).toEqual({ ...flow, action: "approve", scope: "current_call",
      selector: { request_ids: [requestId], expected_request_revisions: { [requestId]: 3 } } });
    return { resolutions: [{ request_id: requestId, outcome: "legacy", decision }] };
  }).flow(flow).approvals;
  const selected = { kind: "requests" as const, ids: [requestId] };
  const oldOwner = await client.observe();
  await expect(client.resolve(oldOwner, selected, "allow_once")).rejects.toMatchObject({ outcome: "not_sent" });
  supportsReceipts = true;
  const review = await client.observe();
  await expect(client.resolve(oldOwner, selected, "allow_once")).rejects.toMatchObject({ outcome: "not_sent" });
  expect(writes).toBe(0);
  expect((await client.resolve(review, selected, "allow_once")).decisions).toEqual([
    { requestId, decisionId: publication },
  ]);
  decision = { action: "denied", decision_id: publication };
  await expect(client.resolve(await client.observe(), selected, "allow_once")).rejects.toMatchObject({ outcome: "unknown" });
  expect(writes).toBe(2);
});

test("compact preview waits and completion leases retain their original Flow and owner edition", async () => {
  const head = { branch: other, version: 4 };
  const known = { host_generation: publication, worker_incarnation: messageId, revision: 7 };
  const lease = { flow, turn: requestId, run: publication, epoch: 3 };
  const snapshot = { flow, head, edition: { ...known, revision: 8 }, text: "Latest output", active: null,
    completion: { lease, outcome: "completed" } };
  let result: unknown = { state: "unchanged", flow, edition: known };
  const client = fixture((request) => {
    expect(request.method).toBe("flow.preview");
    expect(request.params).toEqual({ flow, known });
    return result;
  }).flow(flow);
  expect((await client.preview({ known })).result.state).toBe("unchanged");
  result = { state: "ready", snapshot };
  expect((await client.preview({ known })).result.state).toBe("ready");
  result = { state: "ready", snapshot: { ...snapshot, edition: known } };
  await expect(client.preview({ known })).rejects.toMatchObject({ kind: "source" });
  result = { state: "ready", snapshot: { ...snapshot, completion: { ...snapshot.completion,
    lease: { ...lease, flow: { ...flow, instance_id: other } } } } };
  await expect(client.preview({ known })).rejects.toMatchObject({ kind: "source" });
  result = { state: "unchanged", flow, edition: { ...known, host_generation: other } };
  await expect(client.preview({ known })).rejects.toMatchObject({ kind: "source" });
});

test("draft snapshots and empty waits stay bound to the captured head and owner edition", async () => {
  const head = { branch: other, version: 4 };
  const known = { host_generation: publication, worker_incarnation: messageId, revision: 7 };
  let result: unknown = { state: "unchanged", flow, head, known };
  const client = fixture((request) => {
    expect(request.method).toBe("flow.drafts");
    expect(request.params).toEqual({ flow, head, known });
    return result;
  }).flow(flow);
  expect((await client.drafts({ head, known })).result.state).toBe("unchanged");
  result = { state: "unchanged", flow, head, known: { ...known, revision: 8 } };
  await expect(client.drafts({ head, known })).rejects.toMatchObject({ kind: "source" });
  result = { state: "ready", snapshot: { ...known, flow, head, after: receipt(), calls: [], handoffs: [], truncated: false } };
  expect((await client.drafts({ head, known })).result.state).toBe("ready");
  result = { state: "ready", snapshot: { ...known, flow, head, after: receipt(other), calls: [], handoffs: [], truncated: false } };
  await expect(client.drafts({ head, known })).rejects.toMatchObject({ kind: "source" });
});

test("Flow submission captures target, original request, payload and abort source before negotiation", async () => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const target = { ...flow };
  const submitted = { request_id: requestId, user: { text: "x=y\noriginal" }, invocation: { effort: "high" } };
  let captured: unknown;
  const client = api(async (_url, init) => {
    const request = rpc(init);
    if (request.method === methods.capabilities.name) { await gate; return response(request, catalog()); }
    expect(request.method).toBe(methods["flow.submit"].name);
    captured = request.params;
    return response(request, accepted());
  });
  const bound = client.flow(target);
  const pending = bound.submit(submitted);
  target.instance_id = other;
  submitted.request_id = other;
  submitted.user.text = "new draft";
  submitted.invocation.effort = "none";
  release();
  const delivered = await pending;
  expect(captured).toEqual({ flow, request_id: requestId, user: { text: "x=y\noriginal" }, invocation: { effort: "high" } });
  expect(delivered.flow).toEqual(flow);
  expect(delivered.result.request_id).toBe(requestId);
  expect(submitted.user.text).toBe("new draft");
  expect(Object.isFrozen(bound.address)).toBe(true);

  const stop = new AbortController();
  const replacement = new AbortController();
  const options = { signal: stop.signal };
  const cancelled = bound.submit({ request_id: requestId, user: { text: "cancelled" } }, options);
  options.signal = replacement.signal;
  stop.abort();
  await expect(cancelled).rejects.toMatchObject({ outcome: "not_sent", kind: "aborted" });
});

test("explicit same-ID retries preserve human submission and original durable acceptance after unknown outcome", async () => {
  const requests: Rpc[] = [];
  const original = { request_id: requestId, user: { text: "authored once" } };
  let first = true;
  const client = fixture((request) => {
    requests.push(request);
    if (request.method === methods["flow.submit"].name) {
      if (first) { first = false; throw new Error("response lost after acceptance"); }
      return accepted();
    }
    expect(request.method).toBe(methods["flow.input_status"].name);
    return { flow, request_id: requestId, observation: {
      message_id: messageId, accepted: receipt(), observed_receipt: receipt(), input: { state: "pending" },
    } };
  });
  const bound = client.flow(flow);
  await expect(bound.submit(original)).rejects.toMatchObject({ outcome: "unknown", kind: "transport" });
  expect(requests.length).toBe(1);
  const status = await bound.inputStatus(requestId);
  expect(status.result.observation?.message_id).toBe(messageId);
  expect(requests.length).toBe(2);
  const retried = await bound.submit(original);
  expect(retried.result.acceptance.receipt).toEqual(receipt());
  expect(requests.length).toBe(3);
  expect(requests[0]?.params).toEqual(requests[2]?.params);
  expect(requests.every((request) => request.method !== methods["flow.invoke"].name)).toBe(true);
});

test("late and malformed replies cannot cross Session, Flow, request, publication or receipt boundaries", async () => {
  const status = { flow, request_id: requestId, observation: {
    message_id: messageId, accepted: receipt(), observed_receipt: receipt(),
    input: { state: "committed", receipt: receipt(), lease: { flow, turn: requestId, run: publication, epoch: 1 } },
  } };
  const outputDetail = { publication_id: publication, receipt: receipt(), has_artifact: true,
    artifacts: [{ id: "registered-artifact", bytes: 12, digest: "b".repeat(64) }] };
  const cases: { result: unknown; call: (client: AtmanClient) => Promise<unknown>; outcome: string }[] = [
    { result: { ...sessionView(), session_id: other }, call: (client) => client.openSession(flow.session_id), outcome: "unknown" },
    { result: { catalog: receipt(other), instances: [], total: 0 }, call: (client) => client.session(flow.session_id).flows(), outcome: "not_applicable" },
    { result: { catalog: receipt(flow.session_id), instances: [{ flow: { ...flow, session_id: other }, loaded: false, archived: false }], total: 1 }, call: (client) => client.session(flow.session_id).flows(), outcome: "not_applicable" },
    { result: { flow, receipt: receipt() }, call: (client) => client.session(flow.session_id).bootstrap({ expected_catalog: receipt(flow.session_id) }), outcome: "unknown" },
    { result: { ...accepted(), flow: { ...flow, instance_id: other } }, call: (client) => client.flow(flow).submit({ request_id: requestId, user: { text: "x" } }), outcome: "unknown" },
    { result: { ...accepted(), request_id: other }, call: (client) => client.flow(flow).submit({ request_id: requestId, user: { text: "x" } }), outcome: "unknown" },
    { result: { ...status, request_id: other }, call: (client) => client.flow(flow).inputStatus(requestId), outcome: "not_applicable" },
    { result: { ...status, observation: { ...status.observation, input: { ...status.observation.input, lease: { ...status.observation.input.lease, flow: { ...flow, instance_id: other } } } } }, call: (client) => client.flow(flow).inputStatus(requestId), outcome: "not_applicable" },
    { result: { request_id: other, receipt: receipt() }, call: (client) => client.flow(flow).control({ request_id: requestId, action: "pause", expected_revision: 1 }), outcome: "unknown" },
    { result: { ...outputDetail, publication_id: other }, call: (client) => client.flow(flow).outputDetail({ publication_id: publication }), outcome: "not_applicable" },
    { result: { ...outputDetail, receipt: receipt(flow.instance_id, 2) }, call: (client) => client.flow(flow).outputDetail({ publication_id: publication, expected_receipt: receipt() }), outcome: "not_applicable" },
    { result: { ...outputDetail, has_artifact: false }, call: (client) => client.flow(flow).outputDetail({ publication_id: publication }), outcome: "not_applicable" },
    { result: { ...outputDetail, artifacts: [...outputDetail.artifacts, ...outputDetail.artifacts] }, call: (client) => client.flow(flow).outputDetail({ publication_id: publication }), outcome: "not_applicable" },
    { result: { ...outputDetail, artifacts: [{ id: "id", bytes: 1, digest: "wrong" }] }, call: (client) => client.flow(flow).outputDetail({ publication_id: publication }), outcome: "not_applicable" },
    { result: { messages: ["opaque", "opaque"], next: publication, has_more: true }, call: (client) => client.flow(flow).output({ limit: 1 }), outcome: "not_applicable" },
    { result: { messages: [], has_more: true }, call: (client) => client.flow(flow).output(), outcome: "not_applicable" },
    { result: { batches: [{ receipt: receipt(other), facts: [] }], next: receipt(other), has_more: false }, call: (client) => client.flow(flow).events(), outcome: "not_applicable" },
    { result: { batches: [{ receipt: receipt(), facts: [] }], next: receipt(flow.instance_id, 2), has_more: false }, call: (client) => client.flow(flow).events(), outcome: "not_applicable" },
  ];
  for (const entry of cases) {
    let sent = 0;
    const client = fixture(() => { sent++; return entry.result; });
    await expect(entry.call(client)).rejects.toMatchObject({ kind: "source", outcome: entry.outcome });
    expect(sent).toBe(1);
  }
});

test("pages and controls reject invalid requests before dispatch and return opaque data with retained sources", async () => {
  let calls = 0;
  const rejecting = fixture(() => { calls++; throw new Error("must not dispatch"); });
  const bound = rejecting.flow(flow);
  for (const invalid of [
    () => bound.events({ limit: 0 }), () => bound.output({ limit: 51 }),
    () => rejecting.session(flow.session_id).flows({ limit: 51 }),
    () => bound.inputStatus("00000000-0000-0000-0000-000000000000"),
    () => bound.control({ request_id: requestId, action: "release_pause", expected_revision: 1 }),
    () => bound.events({ after: receipt(other) }),
  ]) await expect(invalid()).rejects.toMatchObject({ outcome: "not_sent" });
  expect(calls).toBe(0);
  const client = fixture((request) => {
    if (request.method === methods["flow.output"].name) return { messages: [{ private_runtime_part: ["opaque"] }], next: publication, has_more: false };
    if (request.method === methods["flow.output_detail"].name) return { publication_id: publication, receipt: receipt(), artifacts: [], has_artifact: false };
    return { batches: [{ receipt: receipt(), facts: [{ private_runtime_fact: true }] }], next: receipt(), has_more: false };
  });
  const output = await client.flow(flow).output();
  expect(output.flow).toEqual(flow);
  expect(output.result.messages).toEqual([{ private_runtime_part: ["opaque"] }]);
  expect((await client.flow(flow).outputDetail({ publication_id: publication })).flow).toEqual(flow);
  expect((await client.flow(flow).events()).result.batches[0]?.facts).toEqual([{ private_runtime_fact: true }]);
});

test("SSE decodes real typed batches directly and closing observation sends no execution control", async () => {
  let connections = 0;
  let cancelled = 0;
  let commands = 0;
  const after = { ...receipt() };
  const next = receipt(flow.instance_id, 2, "b");
  const client = api(async (url, init) => {
    if (init?.method === "POST") {
      const request = rpc(init);
      if (request.method !== methods.capabilities.name) commands++;
      return response(request, catalog());
    }
    const address = new URL(String(url));
    expect(address.pathname).toBe("/events");
    expect(address.searchParams.get("session_id")).toBe(flow.session_id);
    expect(address.searchParams.get("instance_id")).toBe(flow.instance_id);
    expect(address.searchParams.get("after")).toBe(cursorId(receipt()));
    expect(address.searchParams.get("limit")).toBeNull();
    expect(address.searchParams.get("token")).toBeNull();
    connections++;
    const batch = { receipt: next, facts: connections === 1 ? [{ opaque: "fact" }] : { wrong: "shape" } };
    const frame = `event: flow_update\nid: ${cursorId(next)}\ndata: ${JSON.stringify(batch)}\n\n`;
    return new Response(new ReadableStream<Uint8Array>({
      start(controller) { controller.enqueue(new TextEncoder().encode(frame)); },
      cancel() { cancelled++; },
    }), { headers: { "content-type": "text/event-stream" } });
  });
  const pending = client.flow(flow).observeEvents({ after });
  after.sequence = 99;
  const observer = await pending;
  expect((await observer.next()).value?.facts).toEqual([{ opaque: "fact" }]);
  expect(observer.last).toEqual(next);
  expect(observer.flow).toEqual(flow);
  await observer.close();
  expect(commands).toBe(0);
  expect(cancelled).toBe(1);
  const malformed = await client.flow(flow).observeEvents({ after: receipt() });
  await expect(malformed.next()).rejects.toMatchObject({ kind: "protocol" });
  expect(malformed.last).toEqual(receipt());
  expect(commands).toBe(0);
  expect(cancelled).toBe(2);
});

test("composer settings and model searches retain the original Flow boundary", async () => {
  const calls: Rpc[] = [];
  const settings = {
    flow, revision: "composer-revision", model: { status: "pending" },
    trust: { mode: "calm", theme: "default", name: "Calm", icon: "", color: "green", description: "" },
  };
  const origins = { model: "session", reasoning: "global", trust_mode: "session", escalation: "global" } as const;
  const catalog = { rows: [{ key: "smart", alias: true, model: "fixture", provider: "vendor" }], truncated: false };
  let foreign = false;
  const client = fixture((request) => {
    calls.push(request);
    return request.params.catalog_query === undefined ? settings
      : { ...settings, flow: foreign ? { ...flow, instance_id: other } : flow, origins, catalog };
  });
  const target = client.session(flow.session_id).flow(flow.instance_id);
  expect((await target.composer()).result.catalog).toBeUndefined();
  const search = await target.composer({ catalog_query: "SMART" });
  expect(search.result.catalog).toEqual(catalog);
  expect(search.result.origins).toEqual(origins);
  expect(calls.map((call) => call.params)).toEqual([{ flow }, { flow, catalog_query: "SMART" }]);
  foreign = true;
  await expect(target.composer({ catalog_query: "SMART" })).rejects.toMatchObject({ kind: "source" });
  await expect(target.composer({ catalog_query: "a".repeat(257) })).rejects.toThrow("Request parameters are invalid");
  expect(calls).toHaveLength(3);
});


test("Inbox pages and edit receipts remain bound to the original Flow, revision and request", async () => {
  const head = { branch: other, version: 0 };
  const row = { id: messageId, revision: receipt(), text: "Original", text_truncated: false,
    image_count: 0, has_quote: false, held: false, separate_turn: false, delivery: "next_turn" as const };
  const page = { flow: { ...flow }, cursor: receipt(), head, items: [row], next_turn_lanes: [[messageId]],
    offset: 0, total: 1, edits_available: true, insertion_available: false };
  const edit = { expected_head: head, request_id: requestId, message_id: messageId,
    expected_revision: row.revision, change: { action: "cancel" as const } };
  const editReply = { flow: { ...flow }, request_id: requestId, message_id: messageId, receipt: receipt(flow.instance_id, 2) };
  let query: unknown = page;
  let command: unknown = editReply;
  const client = fixture(request => {
    if (request.method === "flow.inbox") {
      expect(request.params).toEqual({ flow, offset: 16 });
      return query;
    }
    expect(request.method).toBe("flow.inbox_edit");
    expect(request.params).toEqual({ ...edit, flow });
    return command;
  }).flow(flow);
  expect((await client.inbox(16)).result).toEqual(page);
  query = { ...page, items: [{ ...row, revision: receipt(other) }] };
  await expect(client.inbox(16)).rejects.toMatchObject({ kind: "source" });
  expect((await client.editInbox(edit)).result).toEqual(editReply);
  command = { ...editReply, request_id: other };
  await expect(client.editInbox(edit)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
  command = { ...editReply, receipt: row.revision };
  await expect(client.editInbox(edit)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
});
