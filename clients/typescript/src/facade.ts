import { defineMethod, HttpClient } from "./client.ts";
import type { FlowEventSubscription, RequestOptions, RpcMethod } from "./client.ts";
import { canonicalUuid, durableCursor, flowSource, journalCursor } from "./cursor.ts";
import type { DurableCursor, FlowSource } from "./cursor.ts";
import { ClientError } from "./errors.ts";
import { decodeDto, methods } from "./generated.ts";
import type { Dtos } from "./generated.ts";

export interface SessionResult<Result> {
  readonly session_id: string;
  readonly result: Result;
}

export interface FlowResult<Result> {
  readonly flow: Readonly<FlowSource>;
  readonly result: Result;
}

type Submit = Pick<Dtos["SubmitFlowRequest"], "request_id" | "expected_head" | "user" | "invocation">;
type Control = Pick<Dtos["ControlFlowRequest"], "request_id" | "expected_revision" | "action" | "pause_key">;
type Output = Pick<Dtos["ReadFlowOutputRequest"], "after" | "limit">;
type Detail = Pick<Dtos["ReadFlowOutputDetailRequest"], "publication_id"> & { readonly expected_receipt?: DurableCursor | null };
type Events = Pick<Dtos["ReadFlowEventsRequest"], "limit"> & { readonly after?: DurableCursor | null };
type Conversation = Pick<Dtos["ReadConversationRequest"], "before" | "limit">;

function source(condition: boolean, message: string): asserts condition {
  if (!condition) throw new ClientError("source", message);
}

function sameId(expected: string, actual: string): boolean {
  return canonicalUuid(expected) === canonicalUuid(actual);
}

function sameFlow(expected: FlowSource, actual: FlowSource): void {
  source(sameId(expected.session_id, actual.session_id) && sameId(expected.instance_id, actual.instance_id),
    "Flow reply differs from its original target");
}

function sameReceipt(expected: DurableCursor, actual: DurableCursor): void {
  source(sameId(expected.journal_id, actual.journal_id) && expected.sequence === actual.sequence && expected.digest === actual.digest,
    "Reply receipt differs from its original boundary");
}

function pageLimit(value: number | undefined, maximum: number): number {
  const limit = value === undefined ? 20 : value;
  if (!Number.isSafeInteger(limit) || limit < 1 || limit > maximum) {
    throw new ClientError("request", `Page limit must be between 1 and ${maximum}`, "not_sent");
  }
  return limit;
}

function checked<Parameters, Result>(
  method: RpcMethod<Parameters, Result>,
  validate: (parameters: Parameters) => void,
  validateResult: (parameters: Parameters, result: Result) => void,
): Readonly<RpcMethod<Parameters, Result>> {
  return defineMethod({
    ...method,
    validate(parameters) { method.validate?.(parameters); validate(parameters); },
    validateResult(parameters, result) { method.validateResult?.(parameters, result); validateResult(parameters, result); },
  });
}

function acceptance(flow: FlowSource, value: Dtos["FlowAcceptance"]): void {
  canonicalUuid(value.message_id);
  durableCursor(value.receipt, flow);
  if (value.disposition.state !== "pending") durableCursor(value.disposition.by, flow);
}

function sessionView(id: string, value: Dtos["SessionView"]): void {
  source(sameId(id, value.session_id), "Session reply differs from its original target");
  journalCursor(value.catalog, id);
  if (value.default_flow != null) canonicalUuid(value.default_flow);
}

const create = checked(methods["session.create"], (request) => { canonicalUuid(request.session_id); },
  (request, result) => sessionView(request.session_id, result));
const open = checked(methods["session.open"], (request) => { canonicalUuid(request.session_id); },
  (request, result) => sessionView(request.session_id, result));
const projectRevision = /^[0-9a-f]{64}$/;
const projects = checked(methods["project.list"], (request) => {
  pageLimit(request.limit ?? 50, 50);
  if (request.after != null) {
    source(projectRevision.test(request.after.revision) && request.after.offset > 0,
      "Project cursor has no valid directory boundary");
  }
}, (request, result) => {
  const offset = request.after?.offset ?? 0;
  source(projectRevision.test(result.revision) &&
    (request.after == null || request.after.revision === result.revision),
    "Project page differs from its original directory revision");
  source(result.projects.length <= pageLimit(request.limit ?? 50, 50), "Project page exceeds its requested limit");
  if (result.after != null) {
    source(result.projects.length > 0 && result.after.revision === result.revision &&
      result.after.offset === offset + result.projects.length, "Project page has an unrelated continuation");
  }
});
const flows = checked(methods["session.flows"], (request) => {
  canonicalUuid(request.session_id);
  pageLimit(request.limit, 50);
  if (request.before != null) canonicalUuid(request.before);
}, (request, result) => {
  journalCursor(result.catalog, request.session_id);
  source(result.instances.length <= pageLimit(request.limit, 50), "Flow page exceeds its requested limit");
  for (const row of result.instances) {
    source(sameId(request.session_id, row.flow.session_id), "Flow catalog row belongs to another Session");
    flowSource(row.flow);
  }
  if (result.next != null) canonicalUuid(result.next);
});
const bootstrap = checked(methods["session.bootstrap"], (request) => {
  canonicalUuid(request.session_id);
  journalCursor(request.expected_catalog, request.session_id);
}, (request, result) => {
  sameFlow({ session_id: request.session_id, instance_id: request.session_id }, result.flow);
  durableCursor(result.receipt, result.flow);
  if (result.acceptance != null) acceptance(result.flow, result.acceptance);
});
const submit = checked(methods["flow.submit"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
  if (request.expected_head != null) canonicalUuid(request.expected_head.branch);
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  source(sameId(request.request_id, result.request_id), "Submission reply belongs to another request");
  acceptance(request.flow, result.acceptance);
});
const inputStatus = checked(methods["flow.input_status"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  source(sameId(request.request_id, result.request_id), "Input status belongs to another request");
  const value = result.observation;
  if (value == null) return;
  canonicalUuid(value.message_id);
  durableCursor(value.accepted, request.flow);
  durableCursor(value.observed_receipt, request.flow);
  if (value.input.state !== "pending") durableCursor(value.input.receipt, request.flow);
  if (value.input.state === "committed") {
    sameFlow(request.flow, value.input.lease.flow);
    canonicalUuid(value.input.lease.turn);
    canonicalUuid(value.input.lease.run);
  }
});
const control = checked(methods["flow.control"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
  if (request.action === "release_pause") {
    source(request.pause_key != null, "Release pause requires its original pause identity");
    canonicalUuid(request.pause_key);
  } else source(request.pause_key == null, "A pause identity only applies to release_pause");
}, (request, result) => {
  source(sameId(request.request_id, result.request_id), "Control reply belongs to another request");
  durableCursor(result.receipt, request.flow);
});
const execution = checked(methods["flow.execution"], (request) => {
  flowSource(request.flow);
  if (request.request_id != null) canonicalUuid(request.request_id);
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  const observed = durableCursor(result.cursor, request.flow);
  source(request.request_id == null ? result.request_id == null :
    result.request_id != null && sameId(request.request_id, result.request_id),
  "Execution status belongs to another control request");
  source(result.allowed_actions.length <= 1 && result.allowed_actions.every((action) =>
    action === "recover" ? result.phase === "interrupted" : action === "cancel_turn" && result.loaded &&
      ["running", "waiting", "finishing"].includes(result.phase)),
  "Execution status contains an unrelated action");
  if (result.control == null) return;
  source(request.request_id != null && (result.control.action === "cancel_turn" || result.control.action === "recover"),
    "Execution status contains an unrelated control receipt");
  const accepted = durableCursor(result.control.accepted, request.flow);
  source(accepted.sequence <= observed.sequence, "Control acceptance is ahead of the execution observation");
  if (accepted.sequence === observed.sequence) sameReceipt(accepted, observed);
  if (result.control.applied == null) return;
  const applied = durableCursor(result.control.applied, request.flow);
  source(accepted.sequence <= applied.sequence && applied.sequence <= observed.sequence,
    "Control completion is outside the execution observation");
  if (accepted.sequence === applied.sequence) sameReceipt(accepted, applied);
  if (applied.sequence === observed.sequence) sameReceipt(applied, observed);
});
const output = checked(methods["flow.output"], (request) => {
  flowSource(request.flow);
  pageLimit(request.limit, 50);
  if (request.after != null) canonicalUuid(request.after);
}, (request, result) => {
  source(result.messages.length <= pageLimit(request.limit, 50), "Output page exceeds its requested limit");
  if (result.next != null) canonicalUuid(result.next);
  source(!result.has_more || result.next != null, "Output page has no continuation boundary");
});
const detail = checked(methods["flow.output_detail"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.publication_id);
  if (request.expected_receipt != null) durableCursor(request.expected_receipt, request.flow);
}, (request, result) => {
  source(sameId(request.publication_id, result.publication_id), "Output detail belongs to another publication");
  durableCursor(result.receipt, request.flow);
  if (request.expected_receipt != null) sameReceipt(request.expected_receipt, result.receipt);
  source(result.has_artifact === (result.artifacts.length > 0), "Artifact flag differs from its identities");
  const ids = new Set<string>();
  for (const artifact of result.artifacts) {
    source(artifact.id.trim().length > 0 && !ids.has(artifact.id), "Artifact identity is empty or duplicated");
    source(/^[0-9a-f]{64}$/i.test(artifact.digest), "Artifact digest is invalid");
    ids.add(artifact.id);
  }
});
const events = checked(methods["flow.events"], (request) => {
  flowSource(request.flow);
  pageLimit(request.limit, 128);
  if (request.after != null) durableCursor(request.after, request.flow);
}, (request, result) => {
  source(result.batches.length <= pageLimit(request.limit, 128), "Event page exceeds its requested limit");
  for (const batch of result.batches) durableCursor(batch.receipt, request.flow);
  if (result.next != null) durableCursor(result.next, request.flow);
  source(!result.has_more || result.next != null, "Event page has no continuation boundary");
  const last = result.batches.at(-1)?.receipt ?? request.after;
  if (last != null) {
    source(result.next != null, "Event page lost its captured boundary");
    sameReceipt(last, result.next);
  } else source(result.next == null, "Empty event page has an unrelated boundary");
});
const eventStream = {
  ...methods["flow.events"],
  decodeBatch: (value: unknown) => decodeDto("FlowEventBatch", value),
};

function conversationItemKey(id: Dtos["ConversationItemId"]): string {
  return [id.commit, id.fact, id.item, id.message].join(":");
}

function conversationBoundary(flow: FlowSource, before: Dtos["ConversationBefore"]): void {
  sameFlow(flow, before.source.flow);
  durableCursor(before.source.cursor, flow);
  canonicalUuid(before.source.branch);
  source(sameId(before.source.branch, before.head.branch), "Conversation continuation has an unrelated head");
}

const conversation = checked(methods["flow.conversation"], (request) => {
  flowSource(request.flow);
  pageLimit(request.limit, 32);
  if (request.before != null) conversationBoundary(request.flow, request.before);
}, (request, response) => {
  if (response.state === "capacity") {
    sameFlow(request.flow, response.flow);
    return;
  }
  const { page } = response;
  sameFlow(request.flow, page.flow);
  sameFlow(request.flow, page.source.flow);
  durableCursor(page.source.cursor, request.flow);
  source(sameId(page.source.branch, page.head.branch), "Conversation page has an unrelated head");
  source(page.items.length <= pageLimit(request.limit, 32), "Conversation page exceeds its requested limit");
  if (request.before != null) {
    const expected = request.before;
    sameReceipt(expected.source.cursor, page.source.cursor);
    source(sameId(expected.head.branch, page.head.branch) && expected.head.version === page.head.version,
      "Conversation page changed its original history head");
  }
  if (page.before != null) {
    conversationBoundary(request.flow, page.before);
    sameReceipt(page.source.cursor, page.before.source.cursor);
    source(sameId(page.head.branch, page.before.head.branch) && page.head.version === page.before.head.version,
      "Conversation continuation changed its original history head");
    const first = page.items[0];
    source(first !== undefined && conversationItemKey(first.id) === conversationItemKey(page.before.item),
      "Conversation continuation does not match the oldest row");
  }
});

const composer = checked(methods["flow.composer"], (request) => {
  flowSource(request.flow);
}, (request, response) => {
  sameFlow(request.flow, response.flow);
});

/** An immutable Session target; opening and bootstrapping remain explicit commands. */
export class SessionClient {
  readonly id: string;
  readonly #transport: HttpClient;

  constructor(transport: HttpClient, sessionId: string) {
    this.#transport = transport;
    this.id = canonicalUuid(sessionId);
    Object.freeze(this);
  }

  async create(request: Pick<Dtos["CreateSessionRequest"], "title" | "project_root">, options: RequestOptions = {}): Promise<SessionResult<Dtos["SessionView"]>> {
    return Object.freeze({ session_id: this.id, result: await this.#transport.request(create, { ...request, session_id: this.id }, options) });
  }

  async open(options: RequestOptions = {}): Promise<SessionResult<Dtos["SessionView"]>> {
    return Object.freeze({ session_id: this.id, result: await this.#transport.request(open, { session_id: this.id }, options) });
  }

  async flows(request: Pick<Dtos["ListFlowsRequest"], "before" | "limit"> = {}, options: RequestOptions = {}): Promise<SessionResult<Dtos["FlowListPage"]>> {
    const captured = { ...request, session_id: this.id };
    captured.limit = pageLimit(captured.limit, 50);
    return Object.freeze({ session_id: this.id, result: await this.#transport.request(flows, captured, options) });
  }

  async bootstrap(request: { readonly expected_catalog: DurableCursor }, options: RequestOptions = {}): Promise<SessionResult<Dtos["LaunchFlowResponse"]>> {
    return Object.freeze({ session_id: this.id, result: await this.#transport.request(bootstrap,
      { session_id: this.id, expected_catalog: { ...request.expected_catalog } }, options) });
  }

  flow(instanceId: string): FlowClient {
    return new FlowClient(this.#transport, { session_id: this.id, instance_id: instanceId });
  }
}

/** Source-bound operations over one original Flow, with no client-owned execution state. */
export class FlowClient {
  readonly #address: Readonly<Dtos["FlowAddress"]>;
  readonly #transport: HttpClient;

  constructor(transport: HttpClient, address: FlowSource) {
    this.#transport = transport;
    this.#address = Object.freeze({ ...flowSource(address) });
    Object.freeze(this);
  }

  get address(): Readonly<FlowSource> { return this.#address; }

  async submit(request: Submit, options: RequestOptions = {}): Promise<FlowResult<Dtos["SubmitFlowResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(submit, { ...request, flow: this.#address }, options) });
  }

  async inputStatus(requestId: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["InputStatusResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(inputStatus, { flow: this.#address, request_id: requestId }, options) });
  }

  async conversation(request: Partial<Conversation> = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationResponse"]>> {
    const captured = { ...request, flow: this.#address, limit: pageLimit(request.limit, 32) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(conversation, captured, options) });
  }

  async composer(request: Pick<Dtos["FlowComposerRequest"], "model" | "reasoning"> = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowComposerResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(composer, { ...request, flow: this.#address }, options) });
  }

  async control(request: Control, options: RequestOptions = {}): Promise<FlowResult<Dtos["ControlFlowResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(control, { ...request, flow: this.#address }, options) });
  }

  async execution(requestId?: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowExecutionResponse"]>> {
    const request = { flow: this.#address, ...(requestId === undefined ? {} : { request_id: requestId }) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(execution, request, options) });
  }

  /** Read the original final-answer publication page, retaining opaque message bodies. */
  async output(request: Output = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowOutputPage"]>> {
    const captured = { ...request, flow: this.#address };
    captured.limit = pageLimit(captured.limit, 50);
    return Object.freeze({ flow: this.address, result: await this.#transport.request(output, captured, options) });
  }

  async outputDetail(request: Detail, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowOutputDetail"]>> {
    const { expected_receipt, ...content } = request;
    const captured = { ...content, flow: this.#address,
      ...(expected_receipt === undefined ? {} : { expected_receipt: expected_receipt === null ? null : { ...expected_receipt } }) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(detail, captured, options) });
  }

  async events(request: Events = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowEventPage"]>> {
    const { after, ...content } = request;
    const captured = { ...content, flow: this.#address, limit: pageLimit(content.limit, 128),
      ...(after === undefined ? {} : { after: after === null ? null : { ...after } }) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(events, captured, options) });
  }

  async observeEvents(request: { readonly after?: DurableCursor | null } = {}, options: RequestOptions = {}): Promise<FlowEventSubscription<Dtos["FlowEventBatch"]>> {
    const after = request.after;
    return this.#transport.subscribeFlowEvents(eventStream,
      { flow: this.address, ...(after == null ? {} : { after }) }, options);
  }
}

/** Portable entry points retain explicit targets and caller-owned business request IDs. */
export class AtmanClient {
  readonly #transport: HttpClient;

  constructor(transport: HttpClient) { this.#transport = transport; }

  capabilities(options: RequestOptions = {}) { return this.#transport.capabilities(options); }

  session(sessionId: string): SessionClient { return new SessionClient(this.#transport, sessionId); }
  flow(address: FlowSource): FlowClient { return new FlowClient(this.#transport, address); }

  createSession(request: Dtos["CreateSessionRequest"], options: RequestOptions = {}): Promise<SessionResult<Dtos["SessionView"]>> {
    const { session_id, ...content } = request;
    return this.session(session_id).create(content, options);
  }

  openSession(sessionId: string, options: RequestOptions = {}): Promise<SessionResult<Dtos["SessionView"]>> {
    return this.session(sessionId).open(options);
  }

  listProjects(request: Dtos["ListProjectsRequest"] = {}, options: RequestOptions = {}): Promise<Dtos["ProjectListPage"]> {
    const { after, limit } = request;
    return this.#transport.request(projects, { limit: pageLimit(limit ?? 50, 50),
      ...(after == null ? {} : { after: { ...after } }) }, options);
  }

  listSessions(request: Dtos["ListSessionsRequest"] = {}, options: RequestOptions = {}): Promise<Dtos["ListSessionsResponse"]> {
    return this.#transport.request(methods.list_sessions, request, options);
  }
}
