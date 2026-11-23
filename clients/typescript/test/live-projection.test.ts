import { expect, test } from "bun:test";
import type { Dtos } from "../src/index.ts";
import { leaseKey, project, type DraftCall, type DraftSnapshot, type Input, type Item, type Lease } from "../web/live-projection.ts";

const flow = { session_id: "session", instance_id: "main" };
const head = { branch: "branch", version: 1 };
const lease: Lease = { flow, turn: "turn", run: "run", epoch: 10 };
const cursor = (sequence: number): Dtos["FlowCursor"] => ({ journal_id: "journal", sequence, digest: `digest-${sequence}` });
const itemId = (commit: number) => ({ commit, fact: 0, item: 0, message: 0 });
const value = (text: string) => ({ text, truncated: false });
const draftId = (call: string, part: number) => ({ call, part, attempt: 1 });
const call = (id: string, parts: DraftCall["parts"]): DraftCall => ({
  id, lease, run: "run", attempt: 1, revision: 1, publication_pending: true, parts,
});
function snapshot(sequence: number, overrides: Partial<DraftSnapshot> = {}): DraftSnapshot {
  return {
    flow, head, after: cursor(sequence), host_generation: "host", worker_incarnation: "worker", revision: sequence,
    active: lease, work: { lease, admission: cursor(1), latest_final: null },
    calls: [], final_answer: null, handoffs: [], truncated: false, ...overrides,
  };
}
const authored: Item = {
  id: itemId(1), body: { kind: "message", role: "user", turn: "turn", parts: [{ kind: "text", text: "Start" }] },
  work: { role: "invocation", lease, publication: null },
};
const input = (sequence: number, live?: DraftSnapshot, items: Item[] = [authored]): Input => ({
  flow, head, cursor: cursor(sequence), items, snapshot: live,
});
const firstThinking = call("provider-call", [{
  id: draftId("provider-call", 0), body: { kind: "thinking", value: value("Thinking") },
}]);
const toolNames = ["read", "scan", "read", "grep"];
const toolCall = call("provider-call", [
  ...firstThinking.parts,
  ...toolNames.map((name, index) => ({
    id: draftId("provider-call", index + 1),
    body: { kind: "tool" as const, index, call_id: value(`call-${index}`), name: value(name) },
  })),
]);
const storedTools: Item = {
  id: itemId(2), body: { kind: "message", role: "assistant", turn: "turn", parts: [
    { kind: "thinking", text: "Thinking" }, ...toolNames.map(name => ({ kind: "tool_call" as const, name })),
  ] }, source_parts: [0, 2, 3, 4, 5], work: { role: "work", lease, publication: "provider-publication" },
};
const witness: DraftSnapshot["handoffs"][number] = {
  lease, run: "run", item: itemId(2), receipt: cursor(2), publication: "provider-publication",
  parts: [
    { live: draftId("provider-call", 0), published_parts: [0] },
    { live: draftId("provider-call", 1), published_parts: [2, 3, 4, 5] },
  ],
};

test("the first Thinking and four tools keep one Work through the durable swap", () => {
  const first = project(input(1, snapshot(1, { calls: [firstThinking] })));
  const firstWork = first.rows.find(row => row.kind === "work")!;
  expect(firstWork.units[0]!.parts).toEqual([{ kind: "thinking", text: "Thinking" }]);
  const streamed = project(input(1, snapshot(1, { revision: 2, calls: [toolCall] })), first.state);
  const streamedWork = streamed.rows.find(row => row.kind === "work")!;
  expect(streamedWork.key).toBe(firstWork.key);
  expect(streamedWork.units[0]!.key).toBe(firstWork.units[0]!.key);
  expect(streamedWork.units[1]!.parts).toHaveLength(4);
  const persisted = project(input(2, snapshot(2, { revision: 3, handoffs: [witness] }), [authored, storedTools]), streamed.state);
  const works = persisted.rows.filter(row => row.kind === "work");
  expect(works).toHaveLength(1);
  expect(works[0]!.key).toBe(firstWork.key);
  expect(works[0]!.units.map(unit => unit.key)).toEqual(streamedWork.units.map(unit => unit.key));
  expect(works[0]!.units.map(unit => unit.origin)).toEqual(["durable", "durable"]);
  expect(project(input(2, undefined, [authored, storedTools]), persisted.state).rows[1]!.units.map(u => u.key))
    .toEqual(works[0]!.units.map(unit => unit.key));
});

test("wrong receipt, full lease, run, ID, ordinal, and publication cannot transfer keys", () => {
  const before = project(input(1, snapshot(1, { calls: [toolCall] })));
  for (const invalid of [
    { ...witness, receipt: { ...cursor(2), digest: "foreign" } },
    { ...witness, lease: { ...lease, epoch: 11 } },
    { ...witness, run: "different-run" },
    { ...witness, publication: "different-publication" },
    { ...witness, item: { ...itemId(2), fact: 1 } },
    { ...witness, parts: witness.parts.map(part => ({ ...part, live: { ...part.live, attempt: 2 } })) },
    { ...witness, parts: witness.parts.map(part => ({ ...part, published_parts: [...part.published_parts, 9] })) },
  ]) {
    const after = project(input(2, snapshot(2, { handoffs: [invalid] }), [authored, storedTools]), before.state);
    expect(after.state.aliases.size).toBe(0);
    expect(after.rows[1]!.units).toHaveLength(2);
  }
});

test("only a typed final folds Work; the authoritative final replaces a same-publication preview even with a bad witness", () => {
  const normal: Item = {
    ...storedTools, body: { kind: "message", role: "assistant", turn: lease.turn, parts: [{ kind: "text", text: "Ordinary" }] },
  };
  const final = call("final-publication", [{ id: draftId("final-publication", 0), body: { kind: "final_answer", value: value("Preview") } }]);
  const actual: Item = {
    ...normal, id: itemId(3), body: { kind: "message", role: "assistant", turn: lease.turn, parts: [{ kind: "text", text: "Actual" }] },
    work: { lease, role: "final_answer", publication: final.id }, source_parts: [0],
  };
  const ordinary = project(input(2, snapshot(2, { calls: [final] }), [authored, normal]));
  expect(ordinary.rows.map(row => row.kind)).toEqual(["item", "work"]);
  expect(ordinary.rows.find(row => row.kind === "work")?.folded).toBe(false);
  const before = project(input(2, snapshot(2, { final_answer: final }), [authored, normal]), ordinary.state);
  const preview = before.rows.find(row => row.kind === "final")!;
  expect(before.rows.find(row => row.kind === "work")?.folded).toBe(true);
  const after = project(input(3, snapshot(3, { final_answer: final, handoffs: [{
    lease, run: lease.run, item: actual.id, receipt: cursor(3), publication: final.id,
    parts: [{ live: draftId(final.id, 0), published_parts: [0] }],
  }] }), [authored, normal, actual]), before.state);
  const durable = after.rows.filter(row => row.kind === "final");
  expect(durable).toHaveLength(1);
  expect(durable[0]!.key).not.toBe(preview.key);
  expect(durable[0]!.units[0]!.key).not.toBe(preview.units[0]!.key);
  expect(durable[0]!.units[0]!.parts).toEqual([{ kind: "text", text: "Actual" }]);
  expect(project(input(3, undefined, [authored, normal, actual])).rows.find(row => row.kind === "work")?.folded).toBe(true);
});

test("a snapshot cannot appear until its exact durable prefix is installed", () => {
  const waiting = project(input(1, snapshot(2, { calls: [firstThinking] })));
  expect(waiting.rows.map(row => row.kind)).toEqual(["item"]);
  const mismatched = project(input(2, snapshot(2, { after: { ...cursor(2), digest: "foreign" }, calls: [firstThinking] })));
  expect(mismatched.rows.map(row => row.kind)).toEqual(["item"]);
  const admitted = project({ ...input(3, snapshot(2, { calls: [firstThinking] })), verifiedCheckpoints: [cursor(2)] });
  expect(admitted.rows.map(row => row.kind)).toEqual(["item", "work"]);
});

test("committed diff-only Work keeps its lease and folds with the final", () => {
  const workDetails = new Set([leaseKey(lease)]);
  const running = project({ ...input(2), workDetails });
  expect(running.rows.map(row => row.kind)).toEqual(["item", "work"]);
  const final: Item = {
    id: itemId(3), body: { kind: "message", role: "assistant", turn: lease.turn, parts: [{ kind: "text", text: "Done" }] },
    work: { role: "final_answer", lease, publication: "final" },
  };
  const finished = project({ ...input(3, undefined, [authored, final]), workDetails }, running.state);
  expect(finished.rows.map(row => row.kind)).toEqual(["item", "work", "final"]);
  expect(finished.rows[1]).toMatchObject({ key: running.rows[1]!.key, units: [], folded: true });
  const foreign = new Set([leaseKey({ ...lease, epoch: lease.epoch + 1 })]);
  expect(project({ ...input(3, undefined, [authored, final]), workDetails: foreign }).rows.map(row => row.kind))
    .toEqual(["item", "final"]);
});

test("an exact final handoff preserves both keys and keeps Final Thinking in Work", () => {
  const final = call("final-id", [
    { id: draftId("final-id", 0), body: { kind: "thinking", value: value("Plan") } },
    { id: draftId("final-id", 1), body: { kind: "final_answer", value: value("Done") } },
  ]);
  const stored: Item = {
    id: itemId(3), body: { kind: "message", role: "assistant", turn: lease.turn, parts: [
      { kind: "thinking", text: "Plan" }, { kind: "text", text: "Done" },
    ] },
    source_parts: [0, 1], work: { lease, role: "final_answer", publication: final.id },
  };
  const handoffs: DraftSnapshot["handoffs"] = [{
    lease, run: lease.run, item: stored.id, receipt: cursor(3), publication: final.id,
    parts: final.parts.map((part, ordinal) => ({ live: part.id, published_parts: [ordinal] })),
  }];
  const before = project(input(2, snapshot(2, { final_answer: final })));
  const after = project(input(3, snapshot(3, { handoffs }), [authored, stored]), before.state);
  expect(after.rows.map(row => row.kind)).toEqual(["item", "work", "final"]);
  expect(after.rows.map(row => row.key)).toEqual(before.rows.map(row => row.key));
  expect(after.rows[1]!.units.map(unit => unit.key)).toEqual(before.rows[1]!.units.map(unit => unit.key));
  expect(after.rows[2]!.units.map(unit => unit.key)).toEqual(before.rows[2]!.units.map(unit => unit.key));
  expect(after.rows.slice(1).flatMap(row => row.units.map(unit => unit.origin))).toEqual(["durable", "durable"]);
  const restarted = project(input(3, snapshot(3, { worker_incarnation: "next", handoffs }), [authored, stored]), before.state);
  expect(restarted.state.aliases.size).toBe(0);
});

test("coalesced publication retires a still-present bounded preview without transferring its key", () => {
  const partial = call("provider-call", toolCall.parts.map(part => part.body.kind === "tool"
    ? { ...part, body: { ...part.body, name: { text: part.body.name.text, truncated: true } } }
    : part));
  const before = project(input(1, snapshot(1, { calls: [partial], truncated: true })));
  const after = project(input(2, snapshot(2, { calls: [partial], handoffs: [witness], truncated: true }), [authored, storedTools]), before.state);
  const work = after.rows.find(row => row.kind === "work")!;
  expect(work.units.map(unit => unit.origin)).toEqual(["durable", "durable"]);
  expect(work.units[0]!.key).toBe(before.rows[1]!.units[0]!.key);
  expect(work.units[1]!.key).not.toBe(before.rows[1]!.units[1]!.key);
});
