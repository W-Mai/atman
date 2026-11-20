import { FlowApprovals } from "./approvals.ts";
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
type Conversation = Pick<Dtos["ReadConversationRequest"], "before" | "limit" | "include_source_parts">;
type HistoryBranches = Omit<Dtos["HistoryBranchesRequest"], "flow" | "offset" | "limit"> &
  Partial<Pick<Dtos["HistoryBranchesRequest"], "offset" | "limit">>;
type HistoryReview = Omit<Dtos["HistoryReviewRequest"], "flow">;
type HistoryRevert = Readonly<{ request_id: string; branch: string }>;
type UserCutoff = Extract<Dtos["ConversationCutoff"], { kind: "before_user_turn" }>;
type HistoryItemSource = Readonly<{
  head: Dtos["HistoryHead"];
  observed: Dtos["HistorySource"];
  cutoff: UserCutoff;
}>;
type Drafts = Pick<Dtos["ReadFlowDraftsRequest"], "head" | "known">;

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

function checkedTurn(expected: FlowSource, actual: Dtos["InputTurn"]): void {
  sameFlow(expected, actual.flow);
  canonicalUuid(actual.turn);
  canonicalUuid(actual.run);
}

function sameTurn(expected: Dtos["InputTurn"], actual: Dtos["InputTurn"]): boolean {
  return sameId(expected.turn, actual.turn) && sameId(expected.run, actual.run) && expected.epoch === actual.epoch;
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
    checkedTurn(request.flow, value.input.lease);
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
const overview = checked(methods["flow.overview"], (request) => {
  flowSource(request.flow);
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  durableCursor(result.cursor, request.flow);
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
  for (const item of page.items) {
    if (item.source_parts != null) {
      source(item.body.kind === "message" && item.source_parts.length === item.body.parts.length,
        "Conversation ordinals do not match the projected parts");
    }
    if (item.work != null) {
      checkedTurn(request.flow, item.work.lease);
      if (item.work.publication != null) canonicalUuid(item.work.publication);
    }
    if (item.user_turn != null) {
      const cutoff = item.user_turn;
      source(cutoff.kind === "before_user_turn" && cutoff.receipt.sequence === item.id.commit,
        "User cutoff differs from its original journal record");
      canonicalUuid(cutoff.input);
      historyLease(request.flow, cutoff.lease);
      durableCursor(cutoff.receipt, request.flow);
      withinHistory(cutoff.receipt, page.source.cursor);
      if (item.body.kind === "message") {
        source(item.body.role === "user" && sameId(item.body.turn, cutoff.lease.turn),
          "User cutoff differs from its displayed authored message");
      }
      if (item.work != null) sameLease(cutoff.lease, item.work.lease);
    }
  }
});

const drafts = checked(methods["flow.drafts"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.head.branch);
  if (request.known != null) {
    canonicalUuid(request.known.host_generation);
    canonicalUuid(request.known.worker_incarnation);
  }
}, (request, result) => {
  const matchesHead = (head: Dtos["HistoryHead"]) =>
    sameId(request.head.branch, head.branch) && request.head.version === head.version;
  if (result.state === "unchanged") {
    sameFlow(request.flow, result.flow);
    const known = request.known;
    source(known != null && matchesHead(result.head) &&
      sameId(known.host_generation, result.known.host_generation) &&
      sameId(known.worker_incarnation, result.known.worker_incarnation) && known.revision === result.known.revision,
    "Draft wait result differs from the requested edition");
    return;
  }
  if (result.state !== "ready") {
    sameFlow(request.flow, result.flow);
    if (result.state === "history_changed") source(!matchesHead(result.head), "Draft history did not change");
    return;
  }
  const { snapshot } = result;
  sameFlow(request.flow, snapshot.flow);
  source(matchesHead(snapshot.head), "Draft snapshot belongs to another history head");
  canonicalUuid(snapshot.host_generation);
  canonicalUuid(snapshot.worker_incarnation);
  const after = durableCursor(snapshot.after, request.flow);
  const checkPrefix = (value: DurableCursor) => {
    const position = durableCursor(value, request.flow);
    source(position.sequence <= after.sequence, "Draft receipt is ahead of its sampled journal");
    if (position.sequence === after.sequence) sameReceipt(position, after);
  };
  const active = snapshot.active;
  if (active != null) checkedTurn(request.flow, active);
  if (snapshot.work != null) {
    checkedTurn(request.flow, snapshot.work.lease);
    source(active != null && sameTurn(active, snapshot.work.lease), "Draft Work belongs to an inactive turn");
    if (snapshot.work.admission != null) checkPrefix(snapshot.work.admission);
    if (snapshot.work.latest_final != null) {
      canonicalUuid(snapshot.work.latest_final.publication);
      checkPrefix(snapshot.work.latest_final.receipt);
    }
  }
  const calls = snapshot.final_answer == null ? snapshot.calls : [...snapshot.calls, snapshot.final_answer];
  for (const call of calls) {
    checkedTurn(request.flow, call.lease);
    canonicalUuid(call.id);
    canonicalUuid(call.run);
    source(active != null && sameTurn(active, call.lease), "Live call belongs to an inactive turn");
    for (const part of call.parts) {
      source(sameId(call.id, part.id.call) && call.attempt === part.id.attempt,
        "Draft part differs from its original call");
    }
  }
  for (const handoff of snapshot.handoffs) {
    checkedTurn(request.flow, handoff.lease);
    canonicalUuid(handoff.publication);
    canonicalUuid(handoff.run);
    checkPrefix(handoff.receipt);
    source(handoff.item.commit === handoff.receipt.sequence, "Draft handoff differs from its original record");
    for (const part of handoff.parts) canonicalUuid(part.live.call);
  }
});

function ordinal(value: number): void {
  source(Number.isSafeInteger(value) && value >= 0, "History revision or offset is outside the client range");
}

function historyHead(value: Dtos["HistoryHead"]): void {
  canonicalUuid(value.branch);
  ordinal(value.version);
}

function sameHead(expected: Dtos["HistoryHead"], actual: Dtos["HistoryHead"]): void {
  historyHead(actual);
  source(sameId(expected.branch, actual.branch) && expected.version === actual.version,
    "History reply differs from its original head");
}

function historySource(flow: FlowSource, value: Dtos["HistorySource"]): void {
  sameFlow(flow, value.flow);
  canonicalUuid(value.branch);
  durableCursor(value.cursor, flow);
}

function withinHistory(receipt: DurableCursor, through: DurableCursor): void {
  source(receipt.sequence <= through.sequence, "History receipt is outside its observed boundary");
  if (receipt.sequence === through.sequence) sameReceipt(through, receipt);
}

function historyLease(flow: FlowSource, lease: Dtos["InputTurn"]): void {
  sameFlow(flow, lease.flow);
  canonicalUuid(lease.turn);
  canonicalUuid(lease.run);
  ordinal(lease.epoch);
}

function sameLease(expected: Dtos["InputTurn"], actual: Dtos["InputTurn"]): void {
  sameFlow(expected.flow, actual.flow);
  source(sameId(expected.turn, actual.turn) && sameId(expected.run, actual.run) && expected.epoch === actual.epoch,
    "History reply differs from the reviewed invocation");
}

function historyIntent(request: Dtos["HistoryReviewRequest"]): void {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
  historyHead(request.expected_head);
  const { target, flow, expected_head } = request;
  source(!sameId(target.branch, expected_head.branch), "History target is already selected");
  if (target.kind === "restore") return;
  historySource(flow, target.observed);
  source(sameId(expected_head.branch, target.observed.branch), "History target uses another observed branch");
  historyLease(flow, target.cutoff.lease);
  durableCursor(target.cutoff.receipt, flow);
  withinHistory(target.cutoff.receipt, target.observed.cursor);
  if (target.cutoff.kind === "before_user_turn") canonicalUuid(target.cutoff.input);
}

function sameHistoryIntent(expected: Dtos["HistoryReviewRequest"], actual: Dtos["HistoryOperationRequest"]): void {
  sameFlow(expected.flow, actual.flow);
  sameHead(expected.expected_head, actual.expected_head);
  source(sameId(expected.request_id, actual.request_id), "History reply belongs to another operation");
  const left = expected.target;
  const right = actual.target;
  source(left.kind === right.kind && sameId(left.branch, right.branch), "History reply changed the requested branch action");
  if (left.kind === "restore" || right.kind === "restore") return;
  sameReceipt(left.observed.cursor, right.observed.cursor);
  sameReceipt(left.cutoff.receipt, right.cutoff.receipt);
  sameLease(left.cutoff.lease, right.cutoff.lease);
  source(left.cutoff.kind === right.cutoff.kind, "History reply changed the selected cutoff");
  if (left.cutoff.kind === "before_user_turn" && right.cutoff.kind === "before_user_turn") {
    source(sameId(left.cutoff.input, right.cutoff.input), "History reply changed the selected User input");
  }
}

function historyConfirmation(request: Dtos["HistoryOperationRequest"]): void {
  historyIntent(request);
  ordinal(request.execution_revision);
  if (request.active != null) historyLease(request.flow, request.active);
  if (request.goal != null) ordinal(request.goal.revision);
  for (const item of request.queued) {
    canonicalUuid(item.message);
    durableCursor(item.revision, request.flow);
  }
}

function historySelection(request: Dtos["HistoryOperationRequest"], selected: Dtos["HistorySelection"]): void {
  const { target, flow } = request;
  source(sameId(target.branch, selected.branch), "History selection differs from the requested branch");
  if (target.kind === "restore") {
    source(selected.kind === "restore", "History selection changed the requested action");
    return;
  }
  source(selected.kind === "create", "History selection changed the requested action");
  historySource(flow, selected.parent);
  source(sameId(target.observed.branch, selected.parent.branch), "History selection changed the source branch");
  if (target.cutoff.kind === "after_completed_turn") sameReceipt(target.cutoff.receipt, selected.parent.cursor);
  else source(selected.parent.cursor.sequence < target.cutoff.receipt.sequence,
    "A before-turn selection must precede the selected input receipt");
}

function historyQuery(request: Dtos["HistoryOperationQuery"]): void {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
}

function historyRecord(query: Dtos["HistoryOperationQuery"], record: Dtos["HistoryOperationRecord"]): void {
  sameFlow(query.flow, record.request.flow);
  source(sameId(query.request_id, record.request.request_id), "History reply belongs to another operation");
  historyConfirmation(record.request);
  historySelection(record.request, record.selection);
  durableCursor(record.accepted, query.flow);
  if (record.cancellation != null) {
    canonicalUuid(record.cancellation.request_id);
    durableCursor(record.cancellation.receipt, query.flow);
    source(record.request.active != null && record.cancellation.receipt.sequence > record.accepted.sequence,
      "Cancellation lacks its reviewed invocation or accepted operation");
  }
  const { status } = record;
  if (status.state === "preparing") return;
  if (status.state === "stopping") {
    source(record.cancellation != null, "Stopping has no exact cancellation acknowledgement");
    return;
  }
  durableCursor(status.receipt, query.flow);
  source(status.receipt.sequence > (record.cancellation?.receipt.sequence ?? record.accepted.sequence),
    "History outcome predates its operation");
  if (status.state === "ready") {
    source((record.request.active != null) === (status.completion != null),
      "History readiness lacks the reviewed invocation completion");
    if (status.completion != null) {
      durableCursor(status.completion, query.flow);
      withinHistory(status.completion, status.receipt);
    }
  }
  if (status.state === "applied") {
    historyHead(status.head);
    source(sameId(record.selection.branch, status.head.branch) &&
      status.head.version === record.request.expected_head.version + 1,
    "Applied history head differs from the reviewed selection");
  }
}

const historyBranches = checked(methods["flow.history_branches"], (request) => {
  flowSource(request.flow);
  historyHead(request.expected_head);
  ordinal(request.offset);
  pageLimit(request.limit, 100);
}, (request, page) => {
  sameFlow(request.flow, page.flow);
  sameHead(request.expected_head, page.head);
  durableCursor(page.observed, request.flow);
  ordinal(page.total);
  const end = request.offset + page.branches.length;
  source(page.branches.length === Math.min(request.limit, page.total - request.offset) &&
    (page.next_offset ?? null) === (end < page.total ? end : null),
  "History branch page has an unrelated continuation");
  for (const branch of page.branches) {
    canonicalUuid(branch.id);
    durableCursor(branch.created, request.flow);
    withinHistory(branch.created, page.observed);
    if (branch.parent != null) {
      historySource(request.flow, branch.parent);
      source(branch.parent.cursor.sequence < branch.created.sequence, "History branch parent must precede its creation");
    }
  }
});

const historyReview = checked(methods["flow.history_review"], historyIntent, (request, review) => {
  historyConfirmation(review.request);
  sameHistoryIntent(request, review.request);
  historySelection(review.request, review.selection);
  for (const input of review.admitted_inputs) canonicalUuid(input);
  for (const input of review.recoverable_inputs ?? []) canonicalUuid(input);
});

const historyBegin = checked(methods["flow.history_begin"], historyConfirmation, (request, record) => {
  historyRecord(request, record);
  const actual = record.request;
  sameHistoryIntent(request, actual);
  source(request.execution_revision === actual.execution_revision &&
    (request.active != null) === (actual.active != null) &&
    request.goal?.id === actual.goal?.id && request.goal?.revision === actual.goal?.revision &&
    request.queued.length === actual.queued.length, "History acknowledgement changed the reviewed operation");
  if (request.active != null && actual.active != null) sameLease(request.active, actual.active);
  for (const [index, item] of request.queued.entries()) {
    const accepted = actual.queued[index];
    source(accepted != null && sameId(item.message, accepted.message), "History acknowledgement changed the reviewed queue");
    sameReceipt(item.revision, accepted.revision);
  }
});

const historyStatus = checked(methods["flow.history_status"], historyQuery, (request, result) => {
  sameFlow(request.flow, result.flow);
  source(sameId(request.request_id, result.request_id), "History status belongs to another operation");
  if (result.operation != null) historyRecord(request, result.operation);
});

const historyRecoverInput = checked(methods["flow.history_recover_input"], (request) => {
  historyQuery(request);
  canonicalUuid(request.input);
  historyHead(request.applied_head);
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  source(sameId(request.request_id, result.request_id) && sameId(request.input, result.input),
    "History recovery belongs to another operation or input");
  sameHead(request.applied_head, result.head);
  durableCursor(result.acceptance, request.flow);
  const candidates = result.recoverable_inputs.map(canonicalUuid);
  source(new Set(candidates).size === candidates.length && candidates.includes(canonicalUuid(result.input)),
    "Recovered input is absent from the original operation");
});

const historyActions = {
  stop: checked(methods["flow.history_stop"], historyQuery, historyRecord),
  apply: checked(methods["flow.history_apply"], historyQuery, (query, record) => {
    historyRecord(query, record);
    source(record.status.state === "applied", "Apply has no committed history head");
  }),
  abort: checked(methods["flow.history_abort"], historyQuery, (query, record) => {
    historyRecord(query, record);
    source(record.status.state === "aborted", "Abort has no committed cancellation of the history operation");
  }),
};

const composer = checked(methods["flow.composer"], (request) => {
  flowSource(request.flow);
}, (request, response) => {
  sameFlow(request.flow, response.flow);
});

const inboxPage = checked(methods["flow.inbox"], (request) => {
  flowSource(request.flow);
  ordinal(request.offset ?? 0);
}, (request, response) => {
  sameFlow(request.flow, response.flow);
  historyHead(response.head);
  const cursor = durableCursor(response.cursor, request.flow);
  ordinal(response.offset);
  ordinal(response.total);
  const start = Math.floor((request.offset ?? 0) / 16) * 16;
  const final = Math.floor(Math.max(0, response.total - 1) / 16) * 16;
  source(response.offset === Math.min(start, final) &&
    response.items.length === Math.min(16, response.total - response.offset), "Inbox page bounds are invalid");
  const ids = new Set<string>();
  for (const item of response.items) {
    const id = canonicalUuid(item.id);
    const revision = durableCursor(item.revision, request.flow);
    source(!ids.has(id) && revision.sequence <= cursor.sequence &&
      (revision.sequence !== cursor.sequence || revision.digest === cursor.digest), "Inbox item identity or revision is invalid");
    ordinal(item.image_count);
    ids.add(id);
  }
  const lanes = response.next_turn_lanes.flatMap(lane => {
    source(lane.length > 0 && lane.length <= 512, "Inbox order is outside the supported range");
    return lane.map(canonicalUuid);
  });
  source(lanes.length <= response.total && new Set(lanes).size === lanes.length, "Inbox order contains repeated or excess inputs");
});

const inboxEdit = checked(methods["flow.inbox_edit"], (request) => {
  flowSource(request.flow);
  historyHead(request.expected_head);
  canonicalUuid(request.request_id);
  canonicalUuid(request.message_id);
  durableCursor(request.expected_revision, request.flow);
  if (request.change.action === "move") {
    const order = request.change.expected_order.map(canonicalUuid);
    source(order.length > 1 && order.length <= 512 && new Set(order).size === order.length &&
      order.includes(canonicalUuid(request.message_id)), "Inbox move requires the original order");
  }
}, (request, response) => {
  sameFlow(request.flow, response.flow);
  source(sameId(request.request_id, response.request_id) && sameId(request.message_id, response.message_id),
    "Inbox reply differs from its original edit");
  source(durableCursor(response.receipt, request.flow).sequence > request.expected_revision.sequence,
    "Inbox edit receipt does not advance the requested revision");
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
    const result = await this.#transport.request(flows, captured, options);
    source(sameId(this.id, result.catalog.journal_id), "Flow directory reply belongs to another Session");
    for (const { flow, map } of result.instances) {
      source(sameId(this.id, flow.session_id), "Flow directory entry belongs to another Session");
      if (map?.parent) {
        source(sameId(this.id, map.parent.session_id) && !sameId(flow.instance_id, map.parent.instance_id),
          "Flow directory parent differs from the visible relationship boundary");
      }
    }
    return Object.freeze({ session_id: this.id, result });
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
  readonly approvals: FlowApprovals;
  readonly #historyItems = new WeakMap<Dtos["ConversationItem"], HistoryItemSource>();

  constructor(transport: HttpClient, address: FlowSource) {
    this.#transport = transport;
    this.#address = Object.freeze({ ...flowSource(address) });
    this.approvals = new FlowApprovals(transport, this.#address);
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
    const result = await this.#transport.request(conversation, captured, options);
    if (result.state === "ready") {
      const { page } = result;
      for (const item of page.items) {
        const cutoff = item.user_turn;
        if (item.body.kind !== "message" || item.body.role !== "user" || cutoff?.kind !== "before_user_turn") continue;
        this.#historyItems.set(item, {
          head: { ...page.head },
          observed: { ...page.source, flow: { ...page.source.flow }, cursor: { ...page.source.cursor } },
          cutoff: { ...cutoff, lease: { ...cutoff.lease, flow: { ...cutoff.lease.flow } }, receipt: { ...cutoff.receipt } },
        });
      }
    }
    return Object.freeze({ flow: this.address, result });
  }

  async drafts(request: Drafts, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadFlowDraftsResponse"]>> {
    const { head, known } = request;
    const captured = { flow: this.#address, head: { ...head }, ...(known == null ? {} : { known: { ...known } }) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(drafts, captured, options) });
  }

  async historyBranches(request: HistoryBranches, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryBranchesResponse"]>> {
    const captured = { ...request, flow: this.#address, offset: request.offset ?? 0, limit: pageLimit(request.limit, 100) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(historyBranches, captured, options) });
  }

  async historyReview(request: HistoryReview, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryReviewResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(historyReview, { ...request, flow: this.#address }, options) });
  }

  canRevert(item: Dtos["ConversationItem"], displayedHead: Dtos["HistoryHead"]): boolean {
    return this.#revertSource(item, displayedHead) !== undefined;
  }

  historyReviewAt(item: Dtos["ConversationItem"], displayedHead: Dtos["HistoryHead"], selection: HistoryRevert,
    options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryReviewResponse"]>> {
    const captured = this.#revertSource(item, displayedHead);
    if (captured === undefined)
      return Promise.reject(new ClientError("request", "User message is outside the displayed conversation head", "not_sent"));
    return this.historyReview({
      request_id: selection.request_id,
      expected_head: captured.head,
      target: { kind: "revert", branch: selection.branch, observed: captured.observed, cutoff: captured.cutoff },
    }, options);
  }

  #revertSource(item: Dtos["ConversationItem"], displayedHead: Dtos["HistoryHead"]): HistoryItemSource | undefined {
    const captured = this.#historyItems.get(item);
    if (captured === undefined || captured.head.version !== displayedHead.version) return undefined;
    try { return sameId(captured.head.branch, displayedHead.branch) ? captured : undefined; }
    catch { return undefined; }
  }

  async historyBegin(reviewed: Dtos["HistoryOperationRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryOperationRecord"]>> {
    try { sameFlow(this.address, reviewed.flow); }
    catch { throw new ClientError("request", "Reviewed operation belongs to another Flow", "not_sent"); }
    return Object.freeze({ flow: this.address, result: await this.#transport.request(historyBegin, reviewed, options) });
  }

  async historyStatus(requestId: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryStatusResponse"]>> {
    const request = { flow: this.#address, request_id: requestId };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(historyStatus, request, options) });
  }

  async historyRecoverInput(applied: Dtos["HistoryOperationRecord"], input: string,
    options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryRecoveredInput"]>> {
    const { request, status } = applied;
    if (status.state !== "applied" || request.target.kind !== "revert" ||
      request.target.cutoff.kind !== "before_user_turn")
      throw new ClientError("request", "Input recovery requires an applied User Revert", "not_sent");
    const appliedReceipt = { ...status.receipt };
    try {
      historyRecord({ flow: this.#address, request_id: request.request_id }, applied);
    } catch {
      throw new ClientError("request", "History recovery belongs to another Flow or source", "not_sent");
    }
    const result = await this.#transport.request(historyRecoverInput, {
      flow: this.#address, request_id: request.request_id, input, applied_head: { ...status.head },
    }, options);
    withinHistory(result.acceptance, appliedReceipt);
    return Object.freeze({ flow: this.address, result });
  }

  historyStop(requestId: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryOperationRecord"]>> {
    return this.#historyAction("stop", requestId, options);
  }

  historyApply(requestId: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryOperationRecord"]>> {
    return this.#historyAction("apply", requestId, options);
  }

  historyAbort(requestId: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["HistoryOperationRecord"]>> {
    return this.#historyAction("abort", requestId, options);
  }

  async #historyAction(action: keyof typeof historyActions, requestId: string, options: RequestOptions): Promise<FlowResult<Dtos["HistoryOperationRecord"]>> {
    const request = { flow: this.#address, request_id: requestId };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(historyActions[action], request, options) });
  }

  async composer(request: Pick<Dtos["FlowComposerRequest"], "model" | "reasoning" | "catalog_query"> = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowComposerResponse"]>> {
    const result = await this.#transport.request(composer, { ...request, flow: this.#address }, options);
    sameFlow(this.#address, result.flow);
    return Object.freeze({ flow: this.address, result });
  }

  async inbox(offset = 0, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadFlowInboxResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(inboxPage, { flow: this.#address, offset }, options) });
  }

  async editInbox(request: Omit<Dtos["EditFlowInboxRequest"], "flow">, options: RequestOptions = {}): Promise<FlowResult<Dtos["EditFlowInboxResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(inboxEdit, { ...request, flow: this.#address }, options) });
  }

  async control(request: Control, options: RequestOptions = {}): Promise<FlowResult<Dtos["ControlFlowResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(control, { ...request, flow: this.#address }, options) });
  }

  async overview(options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowOverviewResponse"]>> {
    const result = await this.#transport.request(overview, { flow: this.#address }, options);
    return Object.freeze({ flow: this.address, result });
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
