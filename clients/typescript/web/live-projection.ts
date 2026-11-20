import type { Dtos, DurableCursor } from "../src/index.ts";

type Address = Dtos["FlowAddress"];
type Head = Dtos["HistoryHead"];
export type Cursor = DurableCursor;
export type Lease = Dtos["InputTurn"];
type ItemId = Dtos["ConversationItemId"];
export type Part = Dtos["ConversationPart"];
export type Item = Dtos["ConversationItem"];
type DraftId = Dtos["FlowDraftId"];
export type DraftCall = Dtos["FlowDraftCall"];
export type DraftSnapshot = Dtos["FlowDraftSnapshot"];
export type Input = {
  flow: Address;
  head: Head;
  cursor: Cursor;
  items: readonly Item[];
  // These are exact checkpoints verified as ancestors of cursor, not a sequence range.
  verifiedCheckpoints?: readonly Cursor[];
  snapshot?: DraftSnapshot | null | undefined;
};
export type Unit = {
  key: string;
  parts: readonly Part[];
  origin: "durable" | "live";
  final: boolean;
  truncated: boolean;
  authoredItem?: Item;
};
export type Row =
  | { kind: "item"; key: string; item: Item; units: Unit[] }
  | { kind: "work"; key: string; lease: Lease; units: Unit[]; folded: boolean }
  | { kind: "final"; key: string; lease: Lease; publication: string; units: Unit[] };
type DraftUnit = Unit & { live: DraftId; lease: string; run: string; publication: string | undefined };
type DurableUnit = Unit & { ordinals: (number | null)[] };
export type State = {
  source: string;
  lane: string | undefined;
  revision: number | undefined;
  displayed: Map<string, DraftUnit>;
  aliases: Map<string, string>;
};

const key = (...fields: (number | string)[]) => JSON.stringify(fields);
const addressKey = (a: Address) => key(a.session_id, a.instance_id);
const leaseKey = (l: Lease) => key(addressKey(l.flow), l.turn, l.run, l.epoch);
const cursorKey = (c: Cursor) => key(c.journal_id, c.sequence, c.digest);
const itemKey = (i: ItemId) => key(i.commit, i.fact, i.item, i.message);
const draftKey = (id: DraftId) => key(id.call, id.attempt, id.part);

function draftUnits(call: DraftCall, lane: string, publication?: string): DraftUnit[] {
  const units: DraftUnit[] = [];
  for (let start = 0; start < call.parts.length;) {
    const first = call.parts[start]!;
    const body = first.body;
    let end = start + 1;
    if (body.kind === "tool") {
      while (call.parts[end]?.body.kind === "tool") end++;
    }
    const group = call.parts.slice(start, end);
    start = end;
    if (group.some(({ id }) => id.call !== call.id || id.attempt !== call.attempt)) continue;
    if (body.kind === "final_answer" && publication === undefined) continue;
    const live = first.id;
    units.push({
      key: key("live", lane, draftKey(live)),
      origin: "live",
      final: body.kind === "final_answer",
      live,
      lease: leaseKey(call.lease),
      run: call.run,
      publication,
      truncated: group.some(({ body: value }) => value.kind === "tool"
        ? value.name.truncated || value.call_id.truncated
        : value.value.truncated),
      parts: group.map(({ body: value }) => value.kind === "tool"
        ? { kind: "tool_call", name: value.name.text,
          ...(!value.call_id.truncated && !value.name.truncated && value.approval_key
            ? { approval_key: value.approval_key } : {}) }
        : { kind: value.kind === "final_answer" ? "text" : value.kind, text: value.value.text }),
    });
  }
  return units;
}

function durableUnits(item: Item, id: string): DurableUnit[] {
  if (item.body.kind !== "message") return [];
  const { parts } = item.body;
  const units: DurableUnit[] = [];
  for (let start = 0; start < parts.length;) {
    let end = start + 1;
    if (parts[start]!.kind === "tool_call") {
      while (parts[end]?.kind === "tool_call") end++;
    }
    units.push({
      key: key("durable", id, start),
      origin: "durable",
      final: item.work?.role === "final_answer" && parts[start]!.kind === "text",
      truncated: false,
      parts: parts.slice(start, end),
      ordinals: Array.from({ length: end - start }, (_, i) => item.source_parts?.[start + i] ?? null),
    });
    start = end;
  }
  return units;
}

function exactPreview(live: DraftUnit, durable: DurableUnit, ordinals: readonly number[]): boolean {
  return !live.truncated && live.final === durable.final && live.parts.length === ordinals.length
    && durable.ordinals.length === ordinals.length
    && durable.ordinals.every((value, index) => value === ordinals[index])
    && durable.parts.every((part, index) => {
      const preview = live.parts[index];
      if (!preview || part.kind !== preview.kind) return false;
      if (part.kind === "tool_call" && preview.kind === "tool_call") return part.name === preview.name;
      return "text" in part && "text" in preview && part.text === preview.text;
    });
}

export function project(input: Input, previous?: State): { rows: Row[]; state: State } {
  const { flow, head, cursor, items } = input;
  const source = key(addressKey(flow), head.branch, head.version, cursor.journal_id);
  const old = previous?.source === source ? previous : undefined;
  const checkpoints = new Set([cursor, ...(input.verifiedCheckpoints ?? [])]
    .filter(c => c.journal_id === cursor.journal_id && c.sequence <= cursor.sequence).map(cursorKey));
  const received = input.snapshot;
  const lane = received && key(received.host_generation, received.worker_incarnation);
  const snapshot = received && addressKey(received.flow) === addressKey(flow)
    && received.head.branch === head.branch && received.head.version === head.version
    && checkpoints.has(cursorKey(received.after))
    && (!old || old.lane !== lane || received.revision >= (old.revision ?? 0)) ? received : undefined;
  const previousDrafts = old && (!snapshot || old.lane === lane) ? old.displayed : new Map<string, DraftUnit>();
  const state: State = {
    source,
    lane: snapshot && lane ? lane : old?.lane,
    revision: snapshot?.revision ?? old?.revision,
    displayed: new Map(),
    aliases: new Map(),
  };
  const rows: Row[] = [];
  const workRows = new Map<string, Extract<Row, { kind: "work" }>>();
  const finals = new Map<string, Extract<Row, { kind: "final" }>>();
  const getWork = (lease: Lease) => {
    const id = leaseKey(lease);
    let row = workRows.get(id);
    if (!row) {
      row = { kind: "work", key: key(source, "work", id), lease, units: [], folded: false };
      workRows.set(id, row);
      rows.push(row);
    }
    return row;
  };
  const finalKey = (lease: Lease, publication: string) => key(source, "final", leaseKey(lease), publication);
  const getFinal = (lease: Lease, publication: string) => {
    const id = finalKey(lease, publication);
    let row = finals.get(id);
    if (!row) {
      row = { kind: "final", key: id, lease, publication, units: [] };
      finals.set(id, row);
      rows.push(row);
    }
    return row;
  };
  const handoffs = new Map<string, Dtos["FlowDraftHandoff"][]>();
  for (const witness of snapshot?.handoffs ?? []) {
    if (witness.item.commit !== witness.receipt.sequence || !checkpoints.has(cursorKey(witness.receipt))) continue;
    const id = itemKey(witness.item);
    const entries = handoffs.get(id) ?? [];
    entries.push(witness);
    handoffs.set(id, entries);
  }
  const replaced = new Set<string>();
  const published = new Set<string>();
  for (const item of items) {
    const id = itemKey(item.id);
    const stableId = key(source, id);
    const units = durableUnits(item, stableId);
    const owner = item.work;
    const first = units[0];
    if (owner?.role === "work" && first && item.body.kind === "message" && item.body.role === "user")
      first.authoredItem = item;
    for (const unit of units) {
      let alias = old?.aliases.get(unit.key);
      if (owner?.publication) {
        for (const witness of handoffs.get(id) ?? []) {
          const lease = leaseKey(owner.lease);
          if (lease !== leaseKey(witness.lease) || witness.publication !== owner.publication) continue;
          for (const part of witness.parts) {
            const identity = draftKey(part.live);
            if (part.published_parts.length > 0 && part.published_parts.length === unit.ordinals.length
              && unit.ordinals.every((ordinal, index) => ordinal === part.published_parts[index])) {
              published.add(key(lease, witness.run, identity));
            }
            const live = previousDrafts.get(identity);
            if (alias || !live || replaced.has(live.key) || live.lease !== lease || live.run !== witness.run
              || (live.publication !== undefined && live.publication !== witness.publication)
              || !exactPreview(live, unit, part.published_parts)) continue;
            alias = live.key;
          }
        }
      }
      if (alias) {
        state.aliases.set(unit.key, alias);
        replaced.add(alias);
        unit.key = alias;
      }
    }
    if (owner?.role === "work") {
      const row = getWork(owner.lease);
      row.units.push(...units);
    } else if (owner?.role === "final_answer" && owner.publication && item.body.kind === "message") {
      const row = getWork(owner.lease);
      row.units.push(...units.filter(unit => !unit.final));
      row.folded = true;
      getFinal(owner.lease, owner.publication).units.push(...units.filter(unit => unit.final));
    } else {
      rows.push({ kind: "item", key: stableId, item, units });
    }
  }
  const activeWork = snapshot?.work;
  if (snapshot && lane && activeWork && addressKey(activeWork.lease.flow) === addressKey(flow)) {
    const lease = leaseKey(activeWork.lease);
    const displayed = (units: DraftUnit[], target: Unit[]) => {
      for (const unit of units) {
        const identity = draftKey(unit.live);
        if (published.has(key(unit.lease, unit.run, identity)) || replaced.has(unit.key) || state.displayed.has(identity)) continue;
        state.displayed.set(draftKey(unit.live), unit);
        target.push(unit);
      }
    };
    const workUnits = snapshot.calls.filter(call => leaseKey(call.lease) === lease)
      .flatMap(call => draftUnits(call, lane));
    const final = snapshot.final_answer;
    const provisional = final && leaseKey(final.lease) === lease
      && !finals.has(finalKey(activeWork.lease, final.id))
      && !(activeWork.latest_final?.publication === final.id && checkpoints.has(cursorKey(activeWork.latest_final.receipt)))
      ? draftUnits(final, lane, final.id) : [];
    const visibleFinal = provisional.filter(unit => unit.final);
    if (workUnits.length || provisional.length) {
      const row = getWork(activeWork.lease);
      displayed(workUnits, row.units);
      displayed(provisional.filter(unit => !unit.final), row.units);
      if (visibleFinal.length && final) {
        row.folded = true;
        displayed(visibleFinal, getFinal(activeWork.lease, final.id).units);
      }
    }
  }
  // A missing observation is not a replacement witness. Retain only the last displayed edition for the next trusted observation.
  if (input.snapshot === undefined && old) state.displayed = old.displayed;
  const visibleRows = rows.filter(row => row.kind === "item" || row.units.length > 0);
  for (const row of visibleRows) {
    if (row.kind === "final") row.key = key(row.key, row.units[0]!.key);
  }
  return { rows: visibleRows, state };
}
