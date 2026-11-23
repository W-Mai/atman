import { canonicalUuid, decodeDto, durableCursor } from "../src/index.ts";
import type { Dtos } from "../src/index.ts";
import type { DraftAttachments, DraftBinding, PersistDraftResult } from "./tab-drafts.ts";

const archive = "atman.web.questions.v1";
const maxRows = 64;
const maxBytes = 256 * 1024;
const maxDraftBytes = 64 * 1024;
const encoder = new TextEncoder();

type Position = Dtos["QuestionPosition"];
type StorageAccess = Pick<Storage, "getItem" | "setItem" | "removeItem">;
export type QuestionSource = Pick<Dtos["ConversationPage"], "head" | "source">;
export type QuestionSend = Readonly<{
  id: string;
  edit: number;
  /** Without the authored content this entry is a read-only delivery lookup. */
  user?: Dtos["SubmittedUserContent"];
  observed?: Position;
}>;
export type QuestionDisposal = Readonly<{ id: string; observed: Position }>;
export type QuestionDraft = Readonly<{
  anchor: QuestionSource;
  ids: Readonly<Pick<Dtos["QuestionIdentity"], "question_id" | "request_id">>;
  owner?: string;
  receipt?: Dtos["QuestionStartReceipt"];
  edit: number;
  text?: string;
  attachments?: DraftAttachments;
  pending?: QuestionSend;
  disposal?: QuestionDisposal;
}>;

function key(binding: DraftBinding): string {
  return [binding.scope, binding.flow.session_id, binding.flow.instance_id, binding.branch].join("/");
}

function lookupOnly({ text: _text, attachments: _attachments, pending, ...ids }: QuestionDraft): QuestionDraft {
  if (!pending) return ids;
  const { user: _user, ...lookup } = pending;
  return { ...ids, pending: lookup };
}

function withoutDraft(draft: QuestionDraft): QuestionDraft {
  const { text: _text, attachments: _attachments, ...rest } = draft;
  return rest;
}

function hasOwner(draft: QuestionDraft): boolean { return Boolean(draft.receipt || draft.pending || draft.disposal); }

function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid question draft");
  return value as Record<string, unknown>;
}

function id(value: unknown): string {
  if (typeof value !== "string") throw new Error("Invalid question ID");
  return canonicalUuid(value);
}

function edit(value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw new Error("Invalid question edit");
  return value;
}

function fromSource(value: unknown, binding: DraftBinding): Dtos["HistorySource"] {
  const source = decodeDto("HistorySource", value);
  if (canonicalUuid(source.flow.session_id) !== binding.flow.session_id ||
    canonicalUuid(source.flow.instance_id) !== binding.flow.instance_id || canonicalUuid(source.branch) !== binding.branch) {
    throw new Error("Question source changed");
  }
  durableCursor(source.cursor, binding.flow);
  return source;
}

function position(value: unknown, question: string, host: string): Position {
  const result = decodeDto("QuestionPosition", value);
  if (id(result.flow_id) !== question || id(result.host_generation) !== host || !Number.isSafeInteger(result.sequence) ||
    result.sequence < 1 || !/^[0-9a-f]{64}$/i.test(result.digest)) throw new Error("Invalid temporary question position");
  return result;
}

function decode(raw: unknown, binding: DraftBinding): QuestionDraft {
  const value = record(raw);
  const anchor = record(value.anchor);
  const source = fromSource(anchor.source, binding);
  const head = decodeDto("HistoryHead", anchor.head);
  if (id(head.branch) !== binding.branch || !Number.isSafeInteger(head.version) || head.version < 0)
    throw new Error("Question branch changed");
  const question = record(value.ids);
  const ids = { question_id: id(question.question_id), request_id: id(question.request_id) };
  if (ids.question_id === ids.request_id || ids.question_id === binding.flow.instance_id) throw new Error("Invalid question identity");
  const owner = value.owner === undefined ? undefined : id(value.owner);
  const receipt = value.receipt === undefined ? undefined : decodeDto("QuestionStartReceipt", value.receipt);
  const host = receipt && id(receipt.launch.host_generation);
  if (receipt && host) {
    if (!owner) throw new Error("Missing question owner");
    fromSource(receipt.source, binding);
    position(receipt.launch, ids.question_id, host);
    position(receipt.accepted, ids.question_id, host);
    id(receipt.message_id);
    if (receipt.accepted.sequence < receipt.launch.sequence || (receipt.accepted.sequence === receipt.launch.sequence &&
      receipt.accepted.digest !== receipt.launch.digest)) throw new Error("Invalid question acceptance");
  }
  const generation = edit(value.edit);
  if (value.text !== undefined && typeof value.text !== "string") throw new Error("Invalid question text");
  const attachments = value.attachments === undefined ? undefined : decodeDto("SubmittedUserContent", { ...record(value.attachments), text: "" });
  const pending = value.pending === undefined ? undefined : record(value.pending);
  const pendingId = pending && id(pending.id);
  const pendingEdit = pending && edit(pending.edit);
  if (pending && (pendingEdit! > generation || pendingId === ids.question_id ||
    (pending.observed === undefined ? Boolean(receipt) || pendingId !== ids.request_id : !host || pendingId === ids.request_id)))
    throw new Error("Invalid pending question identity");
  const observed = pending?.observed === undefined ? undefined : position(pending.observed, ids.question_id, host!);
  const user = pending?.user === undefined ? undefined : decodeDto("SubmittedUserContent", pending.user);
  const disposal = value.disposal === undefined ? undefined : record(value.disposal);
  const disposalId = disposal && id(disposal.id);
  if (disposal && (!host || disposalId === ids.question_id || disposalId === ids.request_id)) throw new Error("Invalid discard identity");
  return {
    anchor: { head, source }, ids, edit: generation,
    ...(owner === undefined ? {} : { owner }), ...(receipt === undefined ? {} : { receipt }),
    ...(value.text === undefined ? {} : { text: value.text }),
    ...(attachments ? { attachments: { ...(attachments.quote == null ? {} : { quote: attachments.quote }),
      ...(attachments.images ? { images: attachments.images } : {}) } } : {}),
    ...(pending ? { pending: { id: pendingId!, edit: pendingEdit!, ...(observed ? { observed } : {}), ...(user ? { user } : {}) } } : {}),
    ...(disposal ? { disposal: { id: disposalId!, observed: position(disposal.observed, ids.question_id, host!) } } : {}),
  };
}

/** Stores draft and operation identities per credential, Flow and branch. The host owns the transcript. */
export class QuestionDrafts {
  readonly #rows = new Map<string, QuestionDraft>();

  constructor(readonly storage: StorageAccess | undefined) {
    try {
      const raw = storage?.getItem(archive);
      if (!raw || raw.length > maxBytes || encoder.encode(raw).length > maxBytes) return;
      const rows: unknown = JSON.parse(raw);
      if (!Array.isArray(rows) || rows.length > maxRows) return;
      for (const row of rows) {
        if (!Array.isArray(row) || row.length !== 2 || typeof row[0] !== "string") continue;
        const fields = row[0].split("/");
        if (fields.length !== 4) continue;
        try {
          const [scope, session_id, instance_id, branch] = fields.map(id) as [string, string, string, string];
          const binding = { scope, flow: { session_id, instance_id }, branch, journal: instance_id };
          this.#rows.set(key(binding), decode(row[1], binding));
        } catch { /* An invalid entry cannot supply an operation identity. */ }
      }
    } catch { /* Editing still works in memory. */ }
  }

  get(binding: DraftBinding): QuestionDraft | undefined { return this.#rows.get(key(binding)); }

  save(binding: DraftBinding, draft: QuestionDraft): PersistDraftResult {
    const storage = this.storage;
    if (!storage) return "memory";
    const target = key(binding);
    const previous = new Map(this.#rows);
    previous.delete(target);
    if (previous.size >= maxRows) {
      const unsubmitted = [...previous].find(([, value]) => !hasOwner(value));
      if (!unsubmitted) return "memory";
      previous.delete(unsubmitted[0]);
    }
    const write = (value: QuestionDraft): boolean => {
      const rows = new Map(previous);
      rows.set(target, value);
      let wire = JSON.stringify([...rows]);
      for (const [other, saved] of rows) {
        if (wire.length <= maxBytes && encoder.encode(wire).length <= maxBytes) break;
        if (other === target) continue;
        if (!hasOwner(saved)) rows.delete(other);
        else rows.set(other, lookupOnly(saved));
        wire = JSON.stringify([...rows]);
      }
      if (wire.length > maxBytes || encoder.encode(wire).length > maxBytes) return false;
      try { storage.setItem(archive, wire); } catch { return false; }
      this.#rows.clear();
      for (const row of rows) this.#rows.set(...row);
      return true;
    };
    const text = JSON.stringify(draft);
    if (text.length <= maxDraftBytes && encoder.encode(text).length <= maxDraftBytes && write(draft)) return "saved";
    const withoutEditor = withoutDraft(draft);
    const reduced = JSON.stringify(withoutEditor);
    if (reduced.length <= maxDraftBytes && encoder.encode(reduced).length <= maxDraftBytes && write(withoutEditor)) return "partial";
    if (hasOwner(draft)) return write(lookupOnly(draft)) ? "partial" : "memory";
    this.remove(binding);
    return "memory";
  }

  remove(binding: DraftBinding): void {
    if (!this.storage) return;
    const target = key(binding);
    if (!this.#rows.has(target)) return;
    const rows = [...this.#rows].filter(([candidate]) => candidate !== target);
    try {
      if (rows.length) this.storage.setItem(archive, JSON.stringify(rows));
      else this.storage.removeItem(archive);
      this.#rows.delete(target);
    } catch { /* A later read still revalidates against the original host. */ }
  }
}
