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
type Conversation = Pick<Dtos["ReadConversationRequest"], "before" | "limit" | "include_source_parts" | "include_work_diffs" | "include_work_logs">;
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
type Preview = Pick<Dtos["ReadFlowPreviewRequest"], "known">;
type QuestionKeys = Omit<Dtos["QuestionIdentity"], "source">;
type StartQuestion = QuestionKeys & Omit<Dtos["StartQuestionRequest"], "identity">;
type QuestionStatus = QuestionKeys & Omit<Dtos["QuestionStatusRequest"], "identity">;
type QuestionTranscript = QuestionKeys & Omit<Dtos["ReadQuestionTranscriptRequest"], "identity">;
type QuestionImage = QuestionKeys & Omit<Dtos["QuestionImageLocator"], "question"> & Omit<Dtos["QuestionTarget"], "identity">;
type QuestionObservation = QuestionTranscript & Pick<Dtos["ReadQuestionObservationRequest"], "known">;
type QuestionTarget = QuestionKeys & Omit<Dtos["QuestionTarget"], "identity">;
type QuestionFollowup = QuestionTarget & Omit<Dtos["QuestionFollowupRequest"], "question">;
type QuestionFollowupStatus = QuestionTarget & Omit<Dtos["QuestionFollowupStatusRequest"], "question">;
type QuestionExecution = QuestionTarget & Omit<Dtos["QuestionExecutionRequest"], "question">;
type QuestionStop = QuestionTarget & Omit<Dtos["QuestionStopRequest"], "question">;
type QuestionDiscard = QuestionTarget & Omit<Dtos["QuestionDiscardRequest"], "question">;
type QuestionDiscardStatus = QuestionTarget & Omit<Dtos["QuestionDiscardStatusRequest"], "question">;
type QuestionRefresh = QuestionTarget & Omit<Dtos["QuestionRefreshRequest"], "question">;
type QuestionRefreshStatus = QuestionTarget & Omit<Dtos["QuestionRefreshStatusRequest"], "question">;
type QuestionKeep = QuestionTarget & Omit<Dtos["QuestionKeepRequest"], "question">;
type QuestionKeepStatus = QuestionTarget & Omit<Dtos["QuestionKeepStatusRequest"], "question">;
type QuestionKeepInstalled = QuestionTarget & Omit<Dtos["QuestionKeepInstalledRequest"], "question">;
type QuestionModelCheck = QuestionTarget & Omit<Dtos["QuestionModelCheckRequest"], "question">;
type QuestionModelOptions = Omit<Dtos["QuestionModelOptionsRequest"], "question" | "flow"> & { question?: QuestionTarget };

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
const projectDirectory = checked(methods["project.directory"], (request) => {
  pageLimit(request.limit ?? 50, 50);
  if (request.after != null) {
    source(!request.refresh && projectRevision.test(request.after.revision) && request.after.offset > 0,
      "Project directory cursor has no valid query boundary");
  }
}, (request, result) => {
  if (result.state === "changed") {
    source(request.after != null, "Project directory invalidated an unrelated continuation");
    return;
  }
  if (result.state === "unknown_project") {
    source(request.target.kind === "sessions", "Project directory is missing a requested Project");
    return;
  }
  const { page } = result;
  const { rows } = page;
  source(rows.kind === request.target.kind && page.sort === (request.sort ?? "newest_first") &&
    projectRevision.test(page.revision) && projectRevision.test(page.catalog_revision) &&
    (request.after == null || page.revision === request.after.revision), "Project directory page differs from its original query");
  const count = rows.kind === "sessions" ? rows.sessions.length : rows.projects.length;
  const next = (request.after?.offset ?? 0) + count;
  source(count <= pageLimit(request.limit ?? 50, 50) && next <= page.total && (page.after == null
    ? next === page.total : count > 0 && next < page.total && page.after.offset === next && page.after.revision === page.revision),
  "Project directory page has an unrelated continuation");
  if (rows.kind === "sessions") {
    source(request.target.kind === "sessions" && (rows.project_root ?? null) === (request.target.project_root ?? null),
      "Project Session page differs from its original Project");
    distinctDirectorySessions(rows.sessions);
    return;
  }
  const roots = new Set<string | null>();
  for (const group of rows.projects) {
    const root = group.project_root ?? null;
    source(!roots.has(root) && (group.record == null || group.record.project_root === root) &&
      group.preview.length === Math.min(5, group.session_count), "Project directory group has an unrelated preview");
    roots.add(root);
    distinctDirectorySessions(group.preview);
  }
});

function distinctDirectorySessions(rows: readonly Dtos["ProjectSessionEntry"][]): void {
  const ids = new Set(rows.map(({ kind, id }) => `${kind}/${canonicalUuid(id)}`));
  source(ids.size === rows.length, "Project directory page repeats a Session source");
}

const projectPin = checked(methods["project.set_pinned"], (request) => {
  source(/^[0-9a-f]{16}$/.test(request.fingerprint) && projectRevision.test(request.expected_revision),
    "Project Pin requires an observed Project and directory revision");
}, (request, result) => {
  source(projectRevision.test(result.revision) && result.project.fingerprint === request.fingerprint &&
    result.project.pinned === request.pinned, "Project Pin receipt differs from the requested state");
});
const workspaceRootId = /^[a-z0-9_-]{1,48}$/i;
const workspaceEncoder = new TextEncoder();

function workspaceComponent(name: string): boolean {
  return workspaceEncoder.encode(name).byteLength <= 255 &&
    name.length > 0 && name !== "~" && !name.startsWith(".") && !/[ .]$/.test(name) &&
    !/[\p{Cc}/\\:<>"|?*]/u.test(name);
}

const workspaceRoots = checked(methods["project.workspaceRoots"], () => {}, (_, result) => {
  if (result.state !== "ready") return;
  const roots = result.directory.roots;
  source(roots.length <= 16 && new Set(roots.map(root => root.id)).size === roots.length &&
    roots.every(root => workspaceRootId.test(root.id) && root.label.trim().length > 0 &&
      workspaceEncoder.encode(root.label).byteLength <= 128 && !/\p{Cc}/u.test(root.label)),
  "Invalid workspace roots directory");
});

const workspaceFolders = checked(methods["project.workspaceFolders"], (request) => {
  source(workspaceRootId.test(request.root_id) && request.components.length <= 32 &&
    request.components.every(workspaceComponent) &&
    workspaceEncoder.encode(request.components.join("/") + "/").byteLength <= 4096,
  "Invalid workspace root or folder components");
  pageLimit(request.limit ?? 50, 50);
  if (request.after != null) canonicalUuid(request.after);
}, (request, result) => {
  if (result.state !== "ready") return;
  const { page } = result;
  source(page.root_id === request.root_id && page.components.length === request.components.length &&
    page.components.every((component, index) => component === request.components[index]),
  "Workspace page belongs to another directory");
  source(page.folders.length <= (request.limit ?? 50) && page.folders.every(workspaceComponent) &&
    new Set(page.folders).size === page.folders.length, "Invalid workspace folder listing");
  canonicalUuid(page.selection);
  if (page.after != null) {
    canonicalUuid(page.after);
    source(page.folders.length === (request.limit ?? 50) && page.after !== page.selection && page.after !== request.after,
      "Invalid workspace continuation");
  }
});

function workspaceOpenResponse(operation: string, response: Dtos["WorkspaceOpenResponse"]): void {
  source(sameId(operation, response.operation_id), "Workspace reply belongs to another operation");
  const progress = response.progress;
  if (progress.state === "unseen" || progress.state === "prepared") return;
  source(/^[0-9a-f]{16}$/.test(progress.fingerprint), "Workspace reply has an invalid Project identity");
  if (progress.state === "session_created") journalCursor(progress.catalog, progress.session_id);
  if (progress.state === "conversation_ready") {
    const flow = flowSource(progress.flow);
    source(sameId(flow.session_id, flow.instance_id) && response.failure == null,
      "Workspace reply has no canonical Conversation");
    journalCursor(progress.catalog, flow.session_id);
    durableCursor(progress.receipt, flow);
  }
}

const workspaceOpen = checked(methods["project.openWorkspace"], (request) => {
  source(!sameId(request.operation_id, request.selection), "Workspace open requires independent operation and selection identities");
}, (request, response) => workspaceOpenResponse(request.operation_id, response));

const workspaceOpenStatus = checked(methods["project.workspaceOpenStatus"], (request) => {
  canonicalUuid(request.operation_id);
}, (request, response) => workspaceOpenResponse(request.operation_id, response));

const gitCoAuthorSet = checked(methods["settings.git_co_author.set"], () => {}, (request, result) => {
  source(result.enabled === request.enabled, "Commit co-author receipt differs from the requested value");
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

function conversationBoundary(flow: FlowSource, before: Pick<Dtos["ConversationBefore"], "source" | "head">): void {
  sameFlow(flow, before.source.flow);
  durableCursor(before.source.cursor, flow);
  canonicalUuid(before.source.branch);
  source(sameId(before.source.branch, before.head.branch), "Conversation continuation has an unrelated head");
}

function executionRecordKey(record: Dtos["ConversationDiffRecord"]): string {
  return [record.commit, record.fact, record.item].join(":");
}

function executionRecord(boundary: Dtos["HistorySource"], receipt: DurableCursor, record: Dtos["ConversationDiffRecord"]): void {
  durableCursor(receipt, boundary.flow);
  withinHistory(receipt, boundary.cursor);
  source(receipt.sequence === record.commit && record.commit > 0, "Receipt differs from its execution record");
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
  if (page.work_diffs != null) {
    source(request.include_work_diffs === true, "Unexpected Work diff index");
    const seen = new Set<string>();
    for (const entry of page.work_diffs.entries) {
      executionRecord(page.source, entry.receipt, entry.record);
      checkedTurn(request.flow, entry.lease);
      source(page.items.some(item => item.work && sameTurn(item.work.lease, entry.lease)), "Diff belongs to another Work turn");
      const key = executionRecordKey(entry.record);
      source(!seen.has(key), "Repeated Work diff record");
      seen.add(key);
    }
  }
  if (page.work_logs != null) {
    source(request.include_work_logs === true, "Unexpected Work log index");
    const seen = new Set<string>();
    for (const entry of page.work_logs.entries) {
      executionRecord(page.source, entry.receipt, entry.record);
      checkedTurn(request.flow, entry.lease);
      source(page.items.some(item => item.work && sameTurn(item.work.lease, entry.lease)), "Log belongs to another Work turn");
      const prefix = `bg_${request.flow.session_id}_`;
      const validExtent = entry.bytes == null ? entry.drained == null :
        Number.isSafeInteger(entry.bytes) && entry.bytes >= 0 && entry.drained != null;
      source(entry.handle.startsWith(prefix) && /^(0|[1-9]\d*)$/.test(entry.handle.slice(prefix.length)) &&
        validExtent && !seen.has(entry.handle), "Invalid or repeated Work log");
      seen.add(entry.handle);
    }
  }
});

const conversationDiff = checked(methods["flow.conversation_diff"], ({ locator }) => {
  conversationBoundary(locator.source.flow, locator);
  executionRecord(locator.source, locator.receipt, locator.record);
}, ({ locator, offset, limit }, result) => {
  if (result.state !== "ready") return;
  const actual = result.locator;
  conversationBoundary(locator.source.flow, actual);
  sameReceipt(locator.source.cursor, actual.source.cursor);
  sameHead(locator.head, actual.head);
  sameReceipt(locator.receipt, actual.receipt);
  source(executionRecordKey(locator.record) === executionRecordKey(actual.record), "Diff reply belongs to another event");
  source(result.offset === offset && result.next_offset === offset + result.lines.length &&
    result.lines.length <= limit && result.next_offset <= result.total_lines &&
    (result.next_offset === result.total_lines || result.lines.length > 0), "Invalid diff line window");
});

const conversationLog = checked(methods["flow.conversation_log"], ({ locator, verification }) => {
  conversationBoundary(locator.source.flow, locator);
  historyHead(locator.head);
  executionRecord(locator.source, locator.receipt, locator.record);
  canonicalUuid(verification.id);
}, ({ locator, offset, limit, verification }, result) => {
  source((result.state === "verification_cancelled") === (verification.cancel === true),
    "Log cancellation does not match the request");
  if (result.state !== "ready" && result.state !== "verifying") return;
  const actual = result.locator;
  conversationBoundary(locator.source.flow, actual);
  sameReceipt(locator.source.cursor, actual.source.cursor);
  sameHead(locator.head, actual.head);
  sameReceipt(locator.receipt, actual.receipt);
  source(executionRecordKey(locator.record) === executionRecordKey(actual.record), "Log reply belongs to another checkpoint");
  if (result.state === "verifying") {
    source(result.verified_bytes > 0 && result.verified_bytes < result.total_bytes, "Invalid log verification progress");
    return;
  }
  const bytes = result.next_offset - offset;
  const encoded = new TextEncoder().encode(result.content).byteLength;
  source(result.offset === offset && bytes >= 0 && bytes <= limit && encoded >= bytes && encoded <= bytes * 3 &&
    result.next_offset <= result.total_bytes && result.has_more === (result.next_offset < result.total_bytes) &&
    (!result.has_more || bytes > 0), "Invalid log byte window");
});

const conversationUsage = checked(methods["flow.conversation_usage"], ({ locator }) => {
  conversationBoundary(locator.source.flow, locator);
  historyHead(locator.head);
  historyLease(locator.source.flow, locator.lease);
  const { item } = locator;
  source(item.commit > 0 && item.commit <= locator.source.cursor.sequence && item.item === 0,
    "Usage requires a row in the observed conversation");
}, ({ locator }, result) => {
  if (result.state !== "ready") return;
  const actual = result.locator;
  conversationBoundary(locator.source.flow, actual);
  sameReceipt(locator.source.cursor, actual.source.cursor);
  sameHead(locator.head, actual.head);
  sameLease(locator.lease, actual.lease);
  source(conversationItemKey(locator.item) === conversationItemKey(actual.item), "Usage reply belongs to another conversation row");
  const routes = new Set<string>();
  for (const row of result.routes) {
    const key = JSON.stringify([row.provider, row.model, row.purpose, row.scope, row.source]);
    source(!routes.has(key) && row.calls > 0 && row.failures <= row.calls &&
      row.cache_read_calls <= row.calls && (row.cache_read_calls > 0) === (row.cache_read > 0) &&
      row.cache_read + row.cache_write <= row.input,
      "Usage reply contains an invalid route total");
    routes.add(key);
  }
});

function conversationPart(locator: Dtos["ConversationImageLocator"]): void {
  conversationBoundary(locator.source.flow, locator);
  source(locator.item.commit > 0 && locator.item.commit <= locator.source.cursor.sequence,
    "Part record is outside its observed conversation");
}

function sameConversationPart(
  expected: Dtos["ConversationImageLocator"],
  actual: Dtos["ConversationImageLocator"],
): void {
  conversationBoundary(expected.source.flow, actual);
  sameReceipt(expected.source.cursor, actual.source.cursor);
  sameHead(expected.head, actual.head);
  source(conversationItemKey(expected.item) === conversationItemKey(actual.item) && expected.part === actual.part,
    "Part reply differs from its original conversation record");
}

const conversationImage = checked(methods["flow.conversation_image"], ({ locator }) => conversationPart(locator),
  ({ locator }, result) => {
    if (result.state === "ready") sameConversationPart(locator, result.locator);
  });

const conversationArtifact = checked(methods["flow.conversation_artifact"], ({ locator, verification }) => {
  conversationPart(locator);
  source(/^out_[0-9a-f]{32}$/i.test(locator.id), "Artifact identity is invalid");
  if (verification) canonicalUuid(verification.id);
}, ({ locator, offset, limit, verification }, result) => {
  if (result.state === "verification_cancelled") {
    source(verification?.cancel === true, "Unexpected artifact cancellation");
    return;
  }
  source(!verification?.cancel, "Artifact cancellation was not acknowledged");
  if (result.state !== "ready" && result.state !== "verifying") return;
  const actual = result.locator;
  sameConversationPart(locator, actual);
  source(locator.id === actual.id, "Artifact reply differs from its original result");
  if (result.state === "verifying") {
    source(verification != null && result.verified_bytes > 0 && result.verified_bytes < result.total_bytes,
      "Artifact reply has invalid verification progress");
    return;
  }
  const bytes = new TextEncoder().encode(result.content).byteLength;
  source(bytes <= limit && result.offset === offset && result.next_offset - offset === bytes &&
    result.next_offset <= result.total_bytes && result.has_more === (result.next_offset < result.total_bytes) &&
    (!result.has_more || bytes > 0), "Artifact reply has an invalid byte window");
});

const preview = checked(methods["flow.preview"], (request) => {
  flowSource(request.flow);
  if (request.known != null) {
    canonicalUuid(request.known.host_generation);
    canonicalUuid(request.known.worker_incarnation);
  }
}, (request, result) => {
  if (result.state === "unavailable") {
    sameFlow(request.flow, result.flow);
    return;
  }
  const edition = result.state === "ready" ? result.snapshot.edition : result.edition;
  canonicalUuid(edition.host_generation);
  canonicalUuid(edition.worker_incarnation);
  const known = request.known;
  const sameOwner = known != null && sameId(known.host_generation, edition.host_generation) &&
    sameId(known.worker_incarnation, edition.worker_incarnation);
  if (result.state === "unchanged") {
    sameFlow(request.flow, result.flow);
    source(sameOwner && known.revision === edition.revision, "Preview wait differs from its requested edition");
    return;
  }
  const snapshot = result.snapshot;
  sameFlow(request.flow, snapshot.flow);
  source(!sameOwner || edition.revision > known.revision, "Preview edition did not advance");
  canonicalUuid(snapshot.head.branch);
  if (snapshot.active != null) checkedTurn(request.flow, snapshot.active);
  if (snapshot.completion != null) checkedTurn(request.flow, snapshot.completion.lease);
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
  draftContent(request.flow, snapshot, checkPrefix);
});

type DraftContents<P> = Pick<Dtos["FlowDraftSnapshot"], "active" | "calls" | "final_answer"> & {
  work?: { lease: Dtos["InputTurn"]; admission?: P | null;
    latest_final?: { publication: string; receipt: P } | null } | null;
  handoffs: readonly { lease: Dtos["InputTurn"]; run: string; publication: string; receipt: P;
    item: Dtos["ConversationItemId"]; parts: readonly Dtos["FlowDraftHandoffPart"][] }[];
};

function draftContent<P extends { sequence: number }>(
  flow: FlowSource, snapshot: DraftContents<P>, checkPrefix: (value: P) => void,
): void {
  const active = snapshot.active;
  if (active != null) checkedTurn(flow, active);
  if (snapshot.work != null) {
    checkedTurn(flow, snapshot.work.lease);
    source(active != null && sameTurn(active, snapshot.work.lease), "Draft Work belongs to an inactive turn");
    if (snapshot.work.admission != null) checkPrefix(snapshot.work.admission);
    if (snapshot.work.latest_final != null) {
      canonicalUuid(snapshot.work.latest_final.publication);
      checkPrefix(snapshot.work.latest_final.receipt);
    }
  }
  const calls = snapshot.final_answer == null ? snapshot.calls : [...snapshot.calls, snapshot.final_answer];
  for (const call of calls) {
    checkedTurn(flow, call.lease);
    canonicalUuid(call.id);
    canonicalUuid(call.run);
    source(active != null && sameTurn(active, call.lease), "Live call belongs to an inactive turn");
    for (const part of call.parts) {
      source(sameId(call.id, part.id.call) && call.attempt === part.id.attempt,
        "Draft part differs from its original call");
    }
  }
  for (const handoff of snapshot.handoffs) {
    checkedTurn(flow, handoff.lease);
    canonicalUuid(handoff.publication);
    canonicalUuid(handoff.run);
    checkPrefix(handoff.receipt);
    source(handoff.item.commit === handoff.receipt.sequence, "Draft handoff differs from its original record");
    for (const part of handoff.parts) canonicalUuid(part.live.call);
  }
}

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

function forkCutoff(flow: FlowSource, cutoff: Dtos["ConversationCutoff"], through: DurableCursor): void {
  historyLease(flow, cutoff.lease);
  durableCursor(cutoff.receipt, flow);
  withinHistory(cutoff.receipt, through);
  if (cutoff.kind === "before_user_turn") canonicalUuid(cutoff.input);
}

function forkReviewIntent(request: Dtos["ForkSourceReviewRequest"]): void {
  const { observed, expected_head, cutoff } = request;
  historyHead(expected_head);
  historySource(observed.flow, observed);
  source(sameId(observed.branch, expected_head.branch), "Fork review uses a different branch");
  if (cutoff != null) forkCutoff(observed.flow, cutoff, observed.cursor);
}

const forkSourceReview = checked(methods["flow.fork_source_review"], forkReviewIntent, (request, review) => {
  const { flow, branch, cursor } = request.observed;
  sameHead(request.expected_head, review.head);
  historySource(flow, review.observed);
  historySource(flow, review.source);
  source(sameId(branch, review.observed.branch) && sameId(branch, review.source.branch), "Fork review changed the source branch");
  sameReceipt(cursor, review.observed.cursor);
  withinHistory(review.source.cursor, cursor);
  journalCursor(review.catalog, flow.session_id);
  ordinal(review.excluded_turns);
  ordinal(review.history_records);
  ordinal(review.snapshot_bytes);
  ordinal(review.asset_copy_bytes);
  const selected = review.selected;
  if (request.cutoff != null) {
    source(selected?.kind === request.cutoff.kind, "Fork review changed the requested cutoff");
    sameReceipt(request.cutoff.receipt, selected.receipt);
    sameLease(request.cutoff.lease, selected.lease);
    if (selected.kind === "before_user_turn" && request.cutoff.kind === "before_user_turn")
      source(sameId(selected.input, request.cutoff.input), "Fork review changed the selected User input");
  } else {
    source(selected?.kind !== "before_user_turn", "Fork review returned a different cutoff kind");
  }
  if (selected != null) forkCutoff(flow, selected, cursor);
  if (selected?.kind === "before_user_turn") {
    source(review.admission != null, "Fork review is missing the containing turn admission");
    durableCursor(review.admission, flow);
    withinHistory(review.admission, selected.receipt);
    source(review.admission.sequence === review.source.cursor.sequence + 1, "Fork review changed the containing turn boundary");
  } else {
    source(review.admission == null, "Fork review returned an unrelated turn admission");
    if (selected?.kind === "after_completed_turn") sameReceipt(selected.receipt, review.source.cursor);
  }
});

function forkOutcome(flow: FlowSource, operation: string, outcome: Dtos["ForkOperationResponse"]): void {
  sameFlow(flow, outcome.source);
  source(sameId(operation, outcome.operation_id), "Fork outcome belongs to another operation");
  const progress = outcome.progress;
  if (progress.state === "unseen") return;
  flowSource(progress.destination);
  source(!sameId(progress.destination.session_id, flow.session_id), "Fork destination must be a separate Session");
  if (progress.state === "ready") {
    durableCursor(progress.activation, progress.destination);
    source(/^[0-9a-f]{64}$/.test(progress.publication_digest), "Fork is missing its complete publication digest");
  }
}

const forkCreate = checked(methods["session.fork_create"], (request) => {
  forkReviewIntent(request.review);
  canonicalUuid(request.operation_id);
  const observed = request.review.observed;
  historySource(observed.flow, request.source);
  source(sameId(observed.branch, request.source.branch), "Fork uses a different reviewed branch");
  withinHistory(request.source.cursor, observed.cursor);
  journalCursor(request.catalog, observed.flow.session_id);
  ordinal(request.asset_copy_bytes);
}, (request, outcome) => forkOutcome(request.review.observed.flow, request.operation_id, outcome));

const forkStatus = checked(methods["session.fork_status"], (request) => {
  flowSource(request.source);
  canonicalUuid(request.operation_id);
}, (request, outcome) => forkOutcome(request.source, request.operation_id, outcome));

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

function questionIdentity(identity: Dtos["QuestionIdentity"]): void {
  const target = flowSource(identity.source);
  const question = canonicalUuid(identity.question_id);
  const request = canonicalUuid(identity.request_id);
  source(question !== target.instance_id && question !== request, "Quick Ask identities must be independent");
}

function sameQuestion(expected: Dtos["QuestionIdentity"], actual: Dtos["QuestionIdentity"]): void {
  sameFlow(expected.source, actual.source);
  source(sameId(expected.question_id, actual.question_id) && sameId(expected.request_id, actual.request_id),
    "Quick Ask reply belongs to another request");
}

function questionReceipt(identity: Dtos["QuestionIdentity"], receipt: Dtos["QuestionStartReceipt"]): void {
  historySource(identity.source, receipt.source);
  canonicalUuid(receipt.message_id);
  for (const position of [receipt.launch, receipt.accepted]) {
    questionPosition(identity, receipt.launch.host_generation, position);
  }
  source(receipt.accepted.sequence >= receipt.launch.sequence &&
    (receipt.accepted.sequence !== receipt.launch.sequence || receipt.accepted.digest === receipt.launch.digest),
  "Quick Ask acceptance is outside the original temporary history");
}

function questionPosition(identity: Dtos["QuestionIdentity"], host: string, position: Dtos["QuestionPosition"]): void {
  source(sameId(host, position.host_generation) && sameId(identity.question_id, position.flow_id) &&
    Number.isSafeInteger(position.sequence) && position.sequence > 0 && /^[0-9a-f]{64}$/i.test(position.digest),
    "Quick Ask receipt does not belong to the temporary Flow");
}

const startQuestion = checked(methods["question.start"], (request) => {
  questionIdentity(request.identity);
  canonicalUuid(request.owner_generation);
  historyHead(request.expected_head);
  historySource(request.identity.source, request.observed);
  source(sameId(request.expected_head.branch, request.observed.branch), "Quick Ask source branch changed");
}, (request, result) => {
  sameQuestion(request.identity, result.identity);
  source(sameId(request.owner_generation, result.owner_generation), "Quick Ask Session owner changed");
  questionReceipt(request.identity, result.receipt);
  source(sameId(result.receipt.source.branch, request.expected_head.branch), "Quick Ask capture changed branches");
  withinHistory(request.observed.cursor, result.receipt.source.cursor);
});

const questionStatus = checked(methods["question.status"], (request) => {
  questionIdentity(request.identity);
  if (request.owner_generation != null) canonicalUuid(request.owner_generation);
}, (request, result) => {
  sameQuestion(request.identity, result.identity);
  if (result.owner_generation != null) canonicalUuid(result.owner_generation);
  if (request.owner_generation != null && (result.owner_generation == null ||
      !sameId(request.owner_generation, result.owner_generation))) {
    source(result.attempt.state === "retired", "Quick Ask status belongs to a different Session owner");
  }
  if (result.attempt.state === "accepted") {
    source(result.owner_generation != null, "Quick Ask acceptance requires an original owner");
    questionReceipt(request.identity, result.attempt.receipt);
  }
});

function questionPageRequest(request: Dtos["ReadQuestionTranscriptRequest"]): void {
  questionIdentity(request.identity);
  canonicalUuid(request.owner_generation);
  canonicalUuid(request.host_generation);
  pageLimit(request.limit, 32);
  if (request.after != null) questionPosition(request.identity, request.host_generation, request.after);
}

function questionPageResponse(
  request: Dtos["ReadQuestionTranscriptRequest"], result: Dtos["ReadQuestionTranscriptResponse"],
): void {
  sameQuestion(request.identity, result.identity);
  source(sameId(request.owner_generation, result.owner_generation) &&
    sameId(request.host_generation, result.host_generation), "Quick Ask transcript owner changed");
  const transcript = result.transcript;
  if (transcript.state === "capacity") {
    ordinal(transcript.allowed);
    ordinal(transcript.required_at_least);
    source(transcript.required_at_least > transcript.allowed, "Invalid Quick Ask transcript capacity");
  }
  if (transcript.state !== "ready") return;
  source(transcript.batches.length <= request.limit && (!transcript.has_more || transcript.batches.length > 0),
    "Quick Ask returned an invalid transcript page");
  let last = request.after ?? null;
  for (const batch of transcript.batches) {
    questionPosition(request.identity, request.host_generation, batch.receipt);
    source(batch.receipt.sequence === (last?.sequence ?? 0) + 1, "Quick Ask transcript batches are not consecutive");
    last = batch.receipt;
    const ids = new Set<string>();
    for (const item of batch.items) {
      const { commit, fact, item: index, message } = item.id;
      source(commit === last.sequence && item.user_turn == null, "Quick Ask row belongs to another publication");
      const key = [fact, index, message].join("/");
      for (const field of [fact, index, message]) ordinal(field);
      source(!ids.has(key), "Quick Ask returned a duplicate transcript row");
      ids.add(key);
      if (item.body.kind === "message") source(item.body.role !== "system", "Quick Ask exposed a system message");
      if (item.work != null) checkedTurn({
        session_id: request.identity.source.session_id, instance_id: request.identity.question_id,
      }, item.work.lease);
    }
  }
  if (transcript.next != null) questionPosition(request.identity, request.host_generation, transcript.next);
  source((last?.sequence ?? null) === (transcript.next?.sequence ?? null) && last?.digest === transcript.next?.digest,
    "Quick Ask continuation differs from the last complete publication");
}

const questionTranscript = checked(methods["question.transcript"], questionPageRequest, questionPageResponse);

const questionObserve = checked(methods["question.observe"], ({ page, known }) => {
  questionPageRequest(page);
  if (known != null) {
    source(sameId(known.host_generation, page.host_generation), "Draft edition belongs to another Quick Ask host");
    canonicalUuid(known.worker_incarnation);
    ordinal(known.revision);
  }
}, ({ page, known }, result) => {
  questionPageResponse(page, result.page);
  const draft = result.drafts;
  if (draft == null) return;
  const transcript = result.page.transcript;
  source(transcript.state === "ready" && sameId(draft.edition.host_generation, page.host_generation),
    "Draft snapshot belongs to another Quick Ask owner");
  canonicalUuid(draft.edition.worker_incarnation);
  ordinal(draft.edition.revision);
  if (known != null) {
    source(sameId(known.worker_incarnation, draft.edition.worker_incarnation) && draft.edition.revision > known.revision,
      "Draft snapshot does not follow the observed Quick Ask edition");
  }
  questionPosition(page.identity, page.host_generation, draft.after);
  if (transcript.state === "ready") {
    const installed = [page.after, ...transcript.batches.map(batch => batch.receipt)]
      .find(position => position?.sequence === draft.after.sequence);
    if (installed != null) withinQuestionHistory(installed, draft.after);
  }
  draftContent(questionFlow(page.identity), draft, (position) => {
    questionPosition(page.identity, page.host_generation, position);
    withinQuestionHistory(position, draft.after);
  });
});

function questionTarget(question: Dtos["QuestionTarget"], id?: string | null): void {
  questionIdentity(question.identity);
  canonicalUuid(question.owner_generation);
  canonicalUuid(question.host_generation);
  if (id == null) return;
  const request = canonicalUuid(id);
  source(request !== canonicalUuid(question.identity.request_id) && request !== canonicalUuid(question.identity.question_id),
    "Quick Ask operation requires an independent request identity");
}

function sameQuestionTarget(expected: Dtos["QuestionTarget"], actual: Dtos["QuestionTarget"]): void {
  sameQuestion(expected.identity, actual.identity);
  source(sameId(expected.owner_generation, actual.owner_generation) && sameId(expected.host_generation, actual.host_generation),
    "Quick Ask reply belongs to another owner");
}

const questionImage = checked(methods["question.image"], ({ locator }) => {
  const { question, position, item } = locator;
  questionTarget(question);
  questionPosition(question.identity, question.host_generation, position);
  source(item.commit === position.sequence && item.item === 0, "Quick Ask image is outside its publication");
}, ({ locator }, result) => {
  if (result.state !== "ready") return;
  const actual = result.locator;
  sameQuestionTarget(locator.question, actual.question);
  questionPosition(locator.question.identity, locator.question.host_generation, actual.position);
  source(actual.position.sequence === locator.position.sequence && actual.position.digest === locator.position.digest &&
    conversationItemKey(actual.item) === conversationItemKey(locator.item) && actual.part === locator.part,
    "Quick Ask image reply differs from its original publication");
});

function questionFlow({ source, question_id }: Dtos["QuestionIdentity"]): FlowSource {
  return { session_id: source.session_id, instance_id: question_id };
}

function withinQuestionHistory(receipt: Dtos["QuestionPosition"], through: Dtos["QuestionPosition"]): void {
  source(through.sequence > receipt.sequence || (through.sequence === receipt.sequence && through.digest === receipt.digest),
    "Quick Ask receipt is outside the observed history");
}

function sameQuestionFollowup(
  request: Dtos["QuestionFollowupStatusRequest"], result: Dtos["QuestionFollowupStatusResponse"] | Dtos["QuestionFollowupResponse"],
): void {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.followup_request_id, result.followup_request_id), "Quick Ask follow-up reply changed identity");
}

const questionFollowup = checked(methods["question.followup"], (request) => {
  questionTarget(request.question, request.followup_request_id);
  questionPosition(request.question.identity, request.question.host_generation, request.observed);
}, (request, result) => {
  sameQuestionFollowup(request, result);
  canonicalUuid(result.message_id);
  questionPosition(request.question.identity, request.question.host_generation, result.accepted);
  source(result.accepted.sequence > request.observed.sequence, "Quick Ask acceptance precedes the observed publication");
});

const questionFollowupStatus = checked(methods["question.followup_status"], (request) => {
  questionTarget(request.question, request.followup_request_id);
}, (request, result) => {
  sameQuestionFollowup(request, result);
  if (result.followup.state !== "observed") return;
  const { identity, host_generation } = request.question;
  const { message_id, accepted, observed_receipt, input } = result.followup.observation;
  canonicalUuid(message_id);
  questionPosition(identity, host_generation, accepted);
  questionPosition(identity, host_generation, observed_receipt);
  const receipt = input.state === "pending" ? accepted : input.receipt;
  questionPosition(identity, host_generation, receipt);
  withinQuestionHistory(accepted, receipt);
  withinQuestionHistory(receipt, observed_receipt);
  if (input.state === "committed") checkedTurn(questionFlow(identity), input.lease);
});

const questionExecution = checked(methods["question.execution"], (request) => {
  questionTarget(request.question, request.stop_request_id);
}, (request, result) => {
  sameQuestionTarget(request.question, result.question);
  source(request.stop_request_id == null ? result.stop_request_id == null :
    result.stop_request_id != null && sameId(request.stop_request_id, result.stop_request_id), "Quick Ask stop lookup changed identity");
  if (result.execution.state !== "observed") return;
  const { identity, host_generation } = request.question;
  const { cursor, revision, stoppable, phase, stop } = result.execution.observation;
  questionPosition(identity, host_generation, cursor);
  ordinal(revision);
  if (stoppable != null) {
    source(phase === "running" || phase === "waiting" || phase === "finishing", "Quick Ask execution is not stoppable");
    checkedTurn(questionFlow(identity), stoppable);
    ordinal(stoppable.epoch);
  }
  if (stop != null) {
    source(request.stop_request_id != null, "Quick Ask returned an unrequested stop receipt");
    questionPosition(identity, host_generation, stop.accepted);
    if (stop.applied != null) {
      questionPosition(identity, host_generation, stop.applied);
      source(stop.applied.sequence > stop.accepted.sequence, "Quick Ask stop precedes its application");
    }
    withinQuestionHistory(stop.applied ?? stop.accepted, cursor);
  }
});

const questionStop = checked(methods["question.stop"], (request) => {
  questionTarget(request.question, request.stop_request_id);
  ordinal(request.expected_revision);
  checkedTurn(questionFlow(request.question.identity), request.lease);
  ordinal(request.lease.epoch);
}, (request, result) => {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.stop_request_id, result.stop_request_id), "Quick Ask stop reply changed identity");
  questionPosition(request.question.identity, request.question.host_generation, result.accepted);
});

function sameDiscard(request: Dtos["QuestionDiscardStatusRequest"], result: Dtos["QuestionDiscardResponse"]): void {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.discard_request_id, result.discard_request_id), "Quick Ask discard reply changed identity");
}

const questionDiscard = checked(methods["question.discard"], (request) => {
  questionTarget(request.question, request.discard_request_id);
  questionPosition(request.question.identity, request.question.host_generation, request.observed);
}, (request, result) => {
  sameDiscard(request, result);
  source(result.discard === "discarded", "Quick Ask discard did not finish");
});

const questionDiscardStatus = checked(methods["question.discard_status"], (request) => {
  questionTarget(request.question, request.discard_request_id);
}, sameDiscard);

function refreshReceipt(question: Dtos["QuestionTarget"], receipt: Dtos["QuestionRefreshReceipt"]): void {
  historySource(question.identity.source, receipt.source);
  questionPosition(question.identity, question.host_generation, receipt.applied);
}

const questionRefresh = checked(methods["question.refresh"], (request) => {
  questionTarget(request.question, request.refresh_request_id);
  questionPosition(request.question.identity, request.question.host_generation, request.expected);
  historyHead(request.source_head);
  historySource(request.question.identity.source, request.observed_source);
  source(sameId(request.source_head.branch, request.observed_source.branch), "Quick Ask refresh source branch changed");
}, (request, result) => {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.refresh_request_id, result.refresh_request_id), "Quick Ask refresh reply changed identity");
  refreshReceipt(request.question, result.receipt);
  source(sameId(request.source_head.branch, result.receipt.source.branch), "Quick Ask refresh captured another branch");
  withinHistory(request.observed_source.cursor, result.receipt.source.cursor);
  source(result.receipt.applied.sequence > request.expected.sequence, "Quick Ask refresh did not advance its context");
});

const questionRefreshStatus = checked(methods["question.refresh_status"], (request) => {
  questionTarget(request.question, request.refresh_request_id);
}, (request, result) => {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.refresh_request_id, result.refresh_request_id), "Quick Ask refresh reply changed identity");
  if (result.refresh.state === "applied") refreshReceipt(request.question, result.refresh.receipt);
});

function keepResponse(request: Dtos["QuestionKeepStatusRequest"], result: Dtos["QuestionKeepResponse"]): void {
  sameQuestionTarget(request.question, result.question);
  source(sameId(request.keep_request_id, result.keep_request_id), "Quick Ask Keep reply changed identity");
  const keep = result.keep;
  if (keep.state !== "published" && keep.state !== "installed") return;
  const { flow, source: temporary, destination, outputs_digest } = keep.receipt;
  sameFlow(questionFlow(request.question.identity), flow);
  questionPosition(request.question.identity, request.question.host_generation, temporary);
  durableCursor(destination, flow);
  source(destination.sequence === temporary.sequence && /^blake3:[0-9a-f]{64}$/.test(outputs_digest),
    "Quick Ask Keep receipt does not match its original boundary");
}

const questionKeep = checked(methods["question.keep"], (request) => {
  questionTarget(request.question, request.keep_request_id);
  questionPosition(request.question.identity, request.question.host_generation, request.expected);
}, (request, result) => {
  keepResponse(request, result);
  const keep = result.keep;
  source(keep.state === "pending" || keep.state === "published" || keep.state === "installed", "Quick Ask Keep reply did not identify its publication");
  if (keep.state !== "pending") {
    source(keep.receipt.source.sequence === request.expected.sequence && keep.receipt.source.digest === request.expected.digest,
      "Quick Ask Keep copied another temporary boundary");
  }
});

const questionKeepStatus = checked(methods["question.keep_status"], (request) => {
  questionTarget(request.question, request.keep_request_id);
}, keepResponse);

const questionKeepInstalled = checked(methods["question.keep_installed"], (request) => {
  questionTarget(request.question, request.keep_request_id);
  durableCursor(request.installed, questionFlow(request.question.identity));
}, (request, result) => {
  keepResponse(request, result);
  source(result.keep.state === "installed", "Quick Ask has no installation receipt");
  sameReceipt(request.installed, result.keep.receipt.destination);
});

const questionModelCheck = checked(methods["question.model_check"], (request) => {
  questionTarget(request.question);
  questionPosition(request.question.identity, request.question.host_generation, request.expected);
}, (request, result) => {
  sameQuestionTarget(request.question, result.question);
  questionPosition(request.question.identity, request.question.host_generation, result.checked);
  source(result.checked.sequence === request.expected.sequence && result.checked.digest === request.expected.digest,
    "Quick Ask model check used another transcript boundary");
  source(result.model.trim().length > 0, "Quick Ask model check has no resolved model");
});

const questionModelOptions = checked(methods["question.model_options"], (request) => {
  flowSource(request.flow);
  if (request.question) {
    questionTarget(request.question);
    sameFlow(request.flow, request.question.identity.source);
  }
}, (request, result) => {
  sameFlow(request.flow, result.flow);
  if (request.question && result.question) sameQuestionTarget(request.question, result.question);
  else source(request.question == null && result.question == null, "Quick Ask model source changed");
  if (result.model.status === "ready") source(result.model.key === request.model, "Quick Ask model preview changed its key");
  if (result.catalog) source(request.catalog_query != null && result.catalog.rows.length <= 128, "Quick Ask catalog differs from the requested search");
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
}, composerResult);

const prepareComposer = checked(methods["flow.composer.prepare"], (request) => {
  flowSource(request.flow);
}, composerResult);

function composerResult(request: Dtos["FlowRequest"], response: Dtos["FlowComposerResponse"]): void {
  sameFlow(request.flow, response.flow);
  if (response.edit_revisions) {
    for (const key of ["model", "reasoning", "trust_mode", "escalation"] as const)
      source(/^[0-9a-f]{64}$/.test(response.edit_revisions[key]), "Composer update lacks an original field revision");
  }
}

function composerPreferenceResult(request: Dtos["ComposerPreferenceStatusRequest"], result: Dtos["ComposerPreferenceResponse"]): void {
  sameFlow(request.flow, result.flow);
  source(sameId(request.request_id, result.request_id), "Composer preference reply belongs to another request");
}

const setComposerPreference = checked(methods["flow.composer.preference.set"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
  source(/^[0-9a-f]{64}$/.test(request.expected_revision), "Composer update requires its observed field revision");
}, (request, result) => {
  composerPreferenceResult(request, result);
  source(result.outcome.status !== "not_found", "Composer write has no typed acknowledgement");
});

const composerPreferenceStatus = checked(methods["flow.composer.preference.status"], (request) => {
  flowSource(request.flow);
  canonicalUuid(request.request_id);
}, composerPreferenceResult);

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

  async conversationArtifact(request: Dtos["ReadConversationArtifactRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationArtifactResponse"]>> {
    sameFlow(this.#address, request.locator.source.flow);
    const result = await this.#transport.request(conversationArtifact, request, options);
    return Object.freeze({ flow: this.address, result });
  }

  async conversationDiff(request: Dtos["ReadConversationDiffRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationDiffResponse"]>> {
    sameFlow(this.#address, request.locator.source.flow);
    const result = await this.#transport.request(conversationDiff, request, options);
    return Object.freeze({ flow: this.address, result });
  }

  async conversationLog(request: Dtos["ReadConversationLogRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationLogResponse"]>> {
    sameFlow(this.#address, request.locator.source.flow);
    const result = await this.#transport.request(conversationLog, request, options);
    return Object.freeze({ flow: this.address, result });
  }

  async conversationUsage(request: Dtos["ReadConversationUsageRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationUsageResponse"]>> {
    sameFlow(this.#address, request.locator.source.flow);
    const result = await this.#transport.request(conversationUsage, request, options);
    return Object.freeze({ flow: this.address, result });
  }

  async conversationImage(request: Dtos["ReadConversationImageRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadConversationImageResponse"]>> {
    sameFlow(this.#address, request.locator.source.flow);
    const result = await this.#transport.request(conversationImage, request, options);
    return Object.freeze({ flow: this.address, result });
  }

  async preview(request: Preview = {}, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadFlowPreviewResponse"]>> {
    const { known } = request;
    const captured = { flow: this.#address, ...(known == null ? {} : { known: { ...known } }) };
    return Object.freeze({ flow: this.address, result: await this.#transport.request(preview, captured, options) });
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

  async forkSourceReview(request: Dtos["ForkSourceReviewRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ForkSourceReviewResponse"]>> {
    sameFlow(this.#address, request.observed.flow);
    return Object.freeze({ flow: this.address, result: await this.#transport.request(forkSourceReview, request, options) });
  }

  forkReviewBefore(item: Dtos["ConversationItem"], displayedHead: Dtos["HistoryHead"]): Dtos["ForkSourceReviewRequest"] | undefined {
    const captured = this.#revertSource(item, displayedHead);
    if (captured === undefined) return undefined;
    return { observed: structuredClone(captured.observed), expected_head: { ...captured.head }, cutoff: structuredClone(captured.cutoff) };
  }

  async createFork(request: Dtos["CreateForkRequest"], options: RequestOptions = {}): Promise<FlowResult<Dtos["ForkOperationResponse"]>> {
    sameFlow(this.#address, request.review.observed.flow);
    return Object.freeze({ flow: this.address, result: await this.#transport.request(forkCreate, request, options) });
  }

  async forkStatus(operation_id: string, options: RequestOptions = {}): Promise<FlowResult<Dtos["ForkOperationResponse"]>> {
    return Object.freeze({ flow: this.address, result: await this.#transport.request(forkStatus, { source: this.#address, operation_id }, options) });
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

  async startQuestion(input: StartQuestion, options: RequestOptions = {}): Promise<FlowResult<Dtos["StartQuestionResponse"]>> {
    const { question_id, request_id, ...params } = input;
    const identity = { source: this.#address, question_id, request_id };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(startQuestion, { ...params, identity }, options),
    });
  }

  async questionStatus(input: QuestionStatus, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionStatusResponse"]>> {
    const { question_id, request_id, owner_generation } = input;
    const identity = { source: this.#address, question_id, request_id };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionStatus, { owner_generation: owner_generation ?? null, identity }, options),
    });
  }

  async questionTranscript(input: QuestionTranscript, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadQuestionTranscriptResponse"]>> {
    const { question_id, request_id, ...params } = input;
    const identity = { source: this.#address, question_id, request_id };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionTranscript, { ...params, identity }, options),
    });
  }

  async questionImage(input: QuestionImage, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadQuestionImageResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...part } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    const result = await this.#transport.request(questionImage, { locator: { ...part, question } }, options);
    return Object.freeze({ flow: this.address, result });
  }

  async questionObserve(input: QuestionObservation, options: RequestOptions = {}): Promise<FlowResult<Dtos["ReadQuestionObservationResponse"]>> {
    const { question_id, request_id, known, ...params } = input;
    const identity = { source: this.#address, question_id, request_id };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionObserve, { page: { ...params, identity }, known: known ?? null }, options),
    });
  }

  async questionFollowup(input: QuestionFollowup, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionFollowupResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionFollowup, { ...params, question }, options),
    });
  }

  async questionFollowupStatus(input: QuestionFollowupStatus, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionFollowupStatusResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, followup_request_id } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionFollowupStatus, { question, followup_request_id }, options),
    });
  }

  async questionExecution(input: QuestionExecution, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionExecutionResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, stop_request_id } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionExecution, { question, stop_request_id: stop_request_id ?? null }, options),
    });
  }

  async questionStop(input: QuestionStop, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionStopResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionStop, { ...params, question }, options),
    });
  }

  async questionDiscard(input: QuestionDiscard, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionDiscardResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionDiscard, { ...params, question }, options),
    });
  }

  async questionDiscardStatus(input: QuestionDiscardStatus, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionDiscardResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, discard_request_id } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionDiscardStatus, { question, discard_request_id }, options),
    });
  }

  async questionRefresh(input: QuestionRefresh, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionRefreshResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionRefresh, { ...params, question }, options),
    });
  }

  async questionRefreshStatus(input: QuestionRefreshStatus, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionRefreshStatusResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, refresh_request_id } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionRefreshStatus, { question, refresh_request_id }, options),
    });
  }

  async questionKeep(input: QuestionKeep, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionKeepResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionKeep, { ...params, question }, options),
    });
  }

  async questionKeepStatus(input: QuestionKeepStatus, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionKeepResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, keep_request_id } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionKeepStatus, { question, keep_request_id }, options),
    });
  }

  async questionKeepInstalled(input: QuestionKeepInstalled, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionKeepResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionKeepInstalled, { ...params, question }, options),
    });
  }

  async questionModelCheck(input: QuestionModelCheck, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionModelCheckResponse"]>> {
    const { question_id, request_id, owner_generation, host_generation, ...params } = input;
    const question = { identity: { source: this.#address, question_id, request_id }, owner_generation, host_generation };
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionModelCheck, { ...params, question }, options),
    });
  }

  async questionModelOptions(input: QuestionModelOptions, options: RequestOptions = {}): Promise<FlowResult<Dtos["QuestionModelOptionsResponse"]>> {
    const { question: owner, ...params } = input;
    const question = owner ? { identity: { source: this.#address, question_id: owner.question_id, request_id: owner.request_id },
      owner_generation: owner.owner_generation, host_generation: owner.host_generation } : null;
    return Object.freeze({
      flow: this.address, result: await this.#transport.request(questionModelOptions, { ...params, flow: this.#address, question }, options),
    });
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

  async setComposerPreference(request: Omit<Dtos["SetComposerPreferenceRequest"], "flow">,
    options: RequestOptions = {}): Promise<FlowResult<Dtos["ComposerPreferenceResponse"]>> {
    const result = await this.#transport.request(setComposerPreference, { ...request, flow: this.#address }, options);
    return Object.freeze({ flow: this.address, result });
  }

  async prepareComposer(options: RequestOptions = {}): Promise<FlowResult<Dtos["FlowComposerResponse"]>> {
    const result = await this.#transport.request(prepareComposer, { flow: this.#address }, options);
    return Object.freeze({ flow: this.address, result });
  }

  async composerPreferenceStatus(requestId: string,
    options: RequestOptions = {}): Promise<FlowResult<Dtos["ComposerPreferenceResponse"]>> {
    const result = await this.#transport.request(composerPreferenceStatus, { flow: this.#address, request_id: requestId }, options);
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

  gitCoAuthor(options: RequestOptions = {}): Promise<Dtos["GitCoAuthorSetting"]> {
    return this.#transport.request(methods["settings.git_co_author"], {}, options);
  }

  setGitCoAuthor(enabled: boolean, options: RequestOptions = {}): Promise<Dtos["GitCoAuthorSetting"]> {
    return this.#transport.request(gitCoAuthorSet, { enabled }, options);
  }

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

  readProjectDirectory(request: Dtos["ReadProjectDirectoryRequest"], options: RequestOptions = {}): Promise<Dtos["ReadProjectDirectoryResponse"]> {
    const { after, ...query } = request;
    return this.#transport.request(projectDirectory, { ...query, target: { ...query.target },
      limit: pageLimit(query.limit ?? 50, 50), ...(after == null ? {} : { after: { ...after } }) }, options);
  }

  setProjectPinned(request: Dtos["SetProjectPinnedRequest"], options: RequestOptions = {}): Promise<Dtos["SetProjectPinnedResponse"]> {
    return this.#transport.request(projectPin, request, options);
  }

  listWorkspaceRoots(options: RequestOptions = {}): Promise<Dtos["WorkspaceRootsResponse"]> {
    return this.#transport.request(workspaceRoots, {}, options);
  }

  listWorkspaceFolders(request: Dtos["ListWorkspaceFoldersRequest"], options: RequestOptions = {}): Promise<Dtos["WorkspaceFolderResponse"]> {
    return this.#transport.request(workspaceFolders, request, options);
  }

  openWorkspace(request: Dtos["OpenWorkspaceRequest"], options: RequestOptions = {}): Promise<Dtos["WorkspaceOpenResponse"]> {
    return this.#transport.request(workspaceOpen, request, options);
  }

  workspaceOpenStatus(operationId: string, options: RequestOptions = {}): Promise<Dtos["WorkspaceOpenResponse"]> {
    return this.#transport.request(workspaceOpenStatus, { operation_id: operationId }, options);
  }

  listSessions(request: Dtos["ListSessionsRequest"] = {}, options: RequestOptions = {}): Promise<Dtos["ListSessionsResponse"]> {
    return this.#transport.request(methods.list_sessions, request, options);
  }
}
