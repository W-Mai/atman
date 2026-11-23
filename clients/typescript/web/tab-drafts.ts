import { canonicalUuid, durableCursor, flowSource } from "../src/index.ts";
import type { Dtos, FlowSource } from "../src/index.ts";

const archiveKey = "atman.web.drafts.v1";
const maxEntries = 32;
const maxArchiveBytes = 256 * 1024;
const maxEntryBytes = 64 * 1024;
const encoder = new TextEncoder();
const prefix = '{"version":1,"drafts":[';
const suffix = "]}";

type DraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;
type Head = Dtos["HistoryHead"];
export type DraftAttachments = Readonly<{
  quote?: string;
  images?: readonly Dtos["InlineImage"][];
}>;
export type DraftContent = Readonly<{ text: string; attachments?: DraftAttachments }>;
export type RecoveryIdentity = Readonly<{
  operation: string;
  input: string;
}>;
export type RecoverySlot = RecoveryIdentity & Readonly<{ alternate: DraftContent }>;
export type DraftRecovery = Readonly<{
  selected?: RecoveryIdentity;
  slots: readonly RecoverySlot[];
}>;
export const maxRecoverySlots = 16;

export type DraftBinding = Readonly<{
  scope: string;
  flow: Readonly<FlowSource>;
  journal: string;
  branch: string;
}>;

/** Without text this is a status lookup identity, not an executable retry. */
export type PendingDraft = Readonly<{
  request_id: string;
  head: Head;
  edit: number;
  text?: string;
  attachments?: DraftAttachments;
}>;

export type DraftSnapshot = Readonly<{
  edit: number;
  text?: string;
  attachments?: DraftAttachments;
  pending?: PendingDraft;
  recovery?: DraftRecovery;
}>;

export type PersistDraftResult = "saved" | "partial" | "memory";

interface Row {
  readonly key: string;
  readonly draft: DraftSnapshot;
  readonly wire: string;
  readonly bytes: number;
}

function bindingKey(binding: DraftBinding): string {
  const target = flowSource(binding.flow);
  const journal = canonicalUuid(binding.journal);
  if (journal !== target.instance_id) throw new Error("Draft does not match its Flow journal");
  return [canonicalUuid(binding.scope), target.session_id, target.instance_id, journal,
    canonicalUuid(binding.branch)].join("/");
}

function fromKey(key: string): string | undefined {
  const fields = key.split("/");
  if (fields.length !== 5) return undefined;
  try {
    const [scope, session_id, instance_id, journal, branch] = fields as [string, string, string, string, string];
    return bindingKey({ scope, flow: { session_id, instance_id }, journal, branch });
  } catch {
    return undefined;
  }
}

/** Call only after the fresh page read has been validated for this connection. */
export function bindDraft(
  scope: string,
  selected: FlowSource,
  page: Dtos["ConversationPage"],
): DraftBinding | undefined {
  try {
    const flow = flowSource(selected);
    for (const fromPage of [page.flow, page.source.flow]) {
      const observed = flowSource(fromPage);
      if (observed.session_id !== flow.session_id || observed.instance_id !== flow.instance_id)
        return undefined;
    }
    const branch = canonicalUuid(page.head.branch);
    if (canonicalUuid(page.source.branch) !== branch) return undefined;
    const journal = durableCursor(page.source.cursor, flow).journal_id;
    return { scope: canonicalUuid(scope), flow, journal, branch };
  } catch {
    return undefined;
  }
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : undefined;
}

function counter(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function safeText(value: unknown): string | undefined {
  if (typeof value !== "string" || value.length > maxEntryBytes) return undefined;
  return encoder.encode(value).length <= maxEntryBytes ? value : undefined;
}

const requestFields = new Set(["request_id", "head", "edit", "text", "attachments"]);

function parseAttachments(value: unknown): DraftAttachments | null | undefined {
  if (value === undefined) return;
  const effects = object(value);
  if (!effects || !Object.keys(effects).every((key) => key === "quote" || key === "images")) return null;
  const quote = safeText(effects.quote);
  if (effects.quote !== undefined && quote === undefined) return null;
  const images: Dtos["InlineImage"][] = [];
  if (effects.images !== undefined) {
    if (!Array.isArray(effects.images) || effects.images.length > 16) return null;
    for (const image of effects.images) {
      const item = object(image);
      if (!item) return null;
      const data_base64 = safeText(item.data_base64);
      if (!data_base64 || !/^[A-Za-z0-9+/]+=*$/.test(data_base64)) return null;
      const name = safeText(item.name);
      if (item.name != null && name === undefined) return null;
      images.push(Object.freeze({ data_base64, ...(name === undefined ? {} : { name }) }));
    }
  }
  return Object.freeze({
    ...(quote === undefined ? {} : { quote }),
    ...(images.length === 0 ? {} : { images: Object.freeze(images) }),
  });
}

function recoveryIdentity(value: unknown): DraftRecovery["selected"] {
  const item = object(value);
  if (typeof item?.operation !== "string" || typeof item.input !== "string") return;
  try {
    return { operation: canonicalUuid(item.operation), input: canonicalUuid(item.input) };
  } catch { /* An invalid identity cannot own an alternate draft. */ }
}

export function sameRecovery(a: DraftRecovery["selected"], b: DraftRecovery["selected"]): boolean {
  return a?.operation === b?.operation && a?.input === b?.input;
}

function parseRecovery(value: unknown): DraftRecovery | undefined {
  const state = object(value);
  if (!Array.isArray(state?.slots) || state.slots.length === 0 || state.slots.length > maxRecoverySlots) return;
  const selected = recoveryIdentity(state.selected);
  const slots: RecoverySlot[] = [];
  for (const raw of state.slots) {
    const item = object(raw);
    const identity = recoveryIdentity(item);
    const alternate = object(item?.alternate);
    const text = safeText(alternate?.text);
    const attachments = parseAttachments(alternate?.attachments);
    if (!identity || text === undefined || attachments === null ||
      slots.some((slot) => sameRecovery(slot, identity))) return;
    slots.push({ ...identity, alternate: { text, ...(attachments === undefined ? {} : { attachments }) } });
  }
  if (state.selected !== undefined && (!selected || !slots.some((slot) => sameRecovery(slot, selected)))) return;
  return { slots, ...(selected === undefined ? {} : { selected }) };
}

function displacedDraft(recovery: DraftRecovery | undefined): DraftContent | undefined {
  return recovery?.selected
    ? recovery.slots.find((slot) => sameRecovery(slot, recovery.selected))?.alternate
    : undefined;
}

function parseSnapshot(value: unknown, branch: string): DraftSnapshot | undefined {
  const draft = object(value);
  if (!draft || !counter(draft.edit)) return undefined;
  const text = safeText(draft.text);
  const attachments = parseAttachments(draft.attachments);
  const recovery = parseRecovery(draft.recovery);
  const editable = text === undefined || attachments === null ?
    (draft.pending === undefined ? displacedDraft(recovery) ?? {} : recovery ? { recovery } : {}) :
    { text, ...(attachments === undefined ? {} : { attachments }), ...(recovery ? { recovery } : {}) };
  if (draft.pending === undefined)
    return Object.freeze({ edit: draft.edit, ...editable });

  const pending = object(draft.pending);
  const head = object(pending?.head);
  if (!pending || !head || typeof pending.request_id !== "string" || typeof head.branch !== "string" ||
      !counter(head.version) || !counter(pending.edit) || pending.edit > draft.edit) return undefined;
  let request_id: string;
  try {
    if (canonicalUuid(head.branch) !== branch) return undefined;
    request_id = canonicalUuid(pending.request_id);
  } catch {
    return undefined;
  }
  const original = safeText(pending.text);
  const originalAttachments = parseAttachments(pending.attachments);
  const replayable = original !== undefined && originalAttachments !== null &&
    (original.trim() !== "" || Boolean(originalAttachments?.quote || originalAttachments?.images?.length)) &&
    Object.keys(head).every((key) => key === "version" || key === "branch") &&
    Object.keys(pending).every((key) => requestFields.has(key));
  return Object.freeze({
    edit: draft.edit,
    ...editable,
    pending: Object.freeze({
      request_id,
      head: Object.freeze({ branch, version: head.version }),
      edit: pending.edit,
      ...(replayable ? { text: original, ...(originalAttachments === undefined ? {} : { attachments: originalAttachments }) } : {}),
    }),
  });
}

function encode(key: string, draft: DraftSnapshot): Row {
  const wire = JSON.stringify([key, draft]);
  return { key, draft, wire, bytes: encoder.encode(wire).length };
}

function metadataOnly(row: Row): Row {
  const pending = row.draft.pending!;
  return encode(row.key, Object.freeze({
    edit: row.draft.edit,
    pending: Object.freeze({ request_id: pending.request_id, head: pending.head, edit: pending.edit }),
  }));
}

function fitEntry(row: Row): Row | undefined {
  if (row.bytes <= maxEntryBytes) return row;
  const { edit, pending } = row.draft;
  if (pending === undefined) {
    const { text, attachments } = displacedDraft(row.draft.recovery) ?? row.draft;
    const editable = encode(row.key, { edit, ...(text === undefined ? {} : { text }),
      ...(attachments === undefined ? {} : { attachments }) });
    return editable.bytes <= maxEntryBytes ? editable : encode(row.key, { edit });
  }
  // Prefer preserving the frozen request to persisting newer editor text.
  const { recovery } = row.draft;
  const original = encode(row.key, { edit, pending, ...(recovery ? { recovery } : {}) });
  if (original.bytes <= maxEntryBytes) return original;
  const frozen = encode(row.key, { edit, pending });
  if (frozen.bytes <= maxEntryBytes) return frozen;
  const lookup = metadataOnly(row);
  if (!recovery) return lookup;
  const retained = encode(row.key, { ...lookup.draft, recovery });
  return retained.bytes <= maxEntryBytes ? retained : lookup;
}

function archiveBytes(rows: readonly Row[]): number {
  return prefix.length + suffix.length + Math.max(0, rows.length - 1) +
    rows.reduce((sum, row) => sum + row.bytes, 0);
}

function fitArchive(rows: Row[], target: string): boolean {
  while (rows.length > maxEntries || archiveBytes(rows) > maxArchiveBytes) {
    const editable = rows.findIndex((row) => row.key !== target && row.draft.pending === undefined);
    if (editable !== -1) {
      rows.splice(editable, 1);
      continue;
    }
    if (rows.length > maxEntries) return false;
    const compact = rows.findIndex((row) => row.draft.pending !== undefined &&
      (row.draft.text !== undefined || row.draft.pending.text !== undefined));
    if (compact === -1) return false;
    rows[compact] = metadataOnly(rows[compact]!);
  }
  return true;
}

function load(storage: DraftStorage): Map<string, Row> | undefined {
  let raw: string | null;
  try {
    raw = storage.getItem(archiveKey);
  } catch {
    return undefined;
  }
  const entries = new Map<string, Row>();
  if (raw === null) return entries;
  if (raw.length > maxArchiveBytes || encoder.encode(raw).length > maxArchiveBytes)
    return undefined;
  try {
    const envelope = object(JSON.parse(raw));
    if (envelope?.version !== 1 || !Array.isArray(envelope.drafts) ||
      envelope.drafts.length > maxEntries) return undefined;
    for (const entry of envelope.drafts) {
      if (!Array.isArray(entry) || entry.length !== 2 || typeof entry[0] !== "string")
        continue;
      const key = fromKey(entry[0]);
      if (key === undefined || entries.has(key)) continue;
      const draft = parseSnapshot(entry[1], key.slice(key.lastIndexOf("/") + 1));
      if (!draft || (draft.text === undefined && draft.pending === undefined && draft.edit === 0)) continue;
      const row = fitEntry(encode(key, draft));
      if (row) entries.set(key, row);
    }
    return entries;
  } catch {
    return undefined;
  }
}

/** Per-tab persistence. The owning editor always retains its complete in-memory draft. */
export class TabDrafts {
  readonly #storage: DraftStorage | undefined;
  #rows: Map<string, Row>;

  constructor(storage: DraftStorage | undefined) {
    const restored = storage === undefined ? undefined : load(storage);
    this.#storage = restored === undefined ? undefined : storage;
    this.#rows = restored ?? new Map();
  }

  get(binding: DraftBinding): DraftSnapshot | undefined {
    const state = this.#rows.get(bindingKey(binding))?.draft;
    if (state?.text === undefined && state?.pending?.text !== undefined &&
        state.edit === state.pending.edit) {
      // Omitting redundant text in a full request does not lose the waiting editor value.
      return Object.freeze({ ...state, text: state.pending.text,
        ...(state.pending.attachments === undefined ? {} : { attachments: state.pending.attachments }) });
    }
    return state;
  }

  save(binding: DraftBinding, draft: DraftSnapshot): PersistDraftResult {
    const key = bindingKey(binding);
    const checked = parseSnapshot(draft, key.slice(key.lastIndexOf("/") + 1));
    const other = [...this.#rows.values()].filter((row) => row.key !== key);
    if (checked === undefined || (!checked.text && !checked.attachments && !checked.pending &&
      !checked.recovery && checked.edit === 0)) {
      const cleared = this.#write(other);
      return cleared && checked !== undefined && draft.text === "" ? "saved" : "memory";
    }
    const entry = fitEntry(encode(key, checked));
    if (!entry) {
      this.#write(other);
      return "memory";
    }
    const rows = [...other, entry];
    if (!fitArchive(rows, key) || !this.#write(rows)) {
      if (checked.pending !== undefined) {
        const recovery = [...other, metadataOnly(entry)];
        if (fitArchive(recovery, key) && this.#write(recovery)) return "partial";
        if (this.#rows.get(key)?.draft.pending?.request_id === checked.pending.request_id)
          return "memory";
      }
      // A denied or oversized write must not leave stale text for this binding if deletion still works.
      this.#write(other);
      return "memory";
    }
    const saved = this.get(binding);
    return saved?.text === draft.text && saved?.pending?.text === draft.pending?.text &&
      Boolean(saved?.attachments) === Boolean(draft.attachments) &&
      Boolean(saved?.recovery) === Boolean(draft.recovery) &&
      Boolean(saved?.pending?.attachments) === Boolean(draft.pending?.attachments) ? "saved" : "partial";
  }

  remove(binding: DraftBinding): void {
    const key = bindingKey(binding);
    if (this.#rows.has(key)) this.#write([...this.#rows.values()].filter((row) => row.key !== key));
  }

  #write(rows: readonly Row[]): boolean {
    if (this.#storage === undefined) return false;
    try {
      if (rows.length > 0) this.#storage.setItem(archiveKey, prefix + rows.map((row) => row.wire).join(",") + suffix);
      else this.#storage.removeItem(archiveKey);
      this.#rows = new Map(rows.map((row) => [row.key, row]));
      return true;
    } catch {
      return false;
    }
  }
}
