import { expect, test } from "bun:test";
import { QuestionDrafts } from "../web/question-drafts.ts";
import type { QuestionDraft } from "../web/question-drafts.ts";
import type { DraftBinding } from "../web/tab-drafts.ts";

const scope = crypto.randomUUID();
const flow = { session_id: crypto.randomUUID(), instance_id: crypto.randomUUID() };
const branch = crypto.randomUUID();
const ids = { question_id: crypto.randomUUID(), request_id: crypto.randomUUID() };
const temporary = { flow_id: ids.question_id, host_generation: crypto.randomUUID(), sequence: 2, digest: "a".repeat(64) };
const source = { flow, branch, cursor: { journal_id: flow.instance_id, sequence: 8, digest: "b".repeat(64) } };
const binding = { scope, flow, branch, journal: flow.instance_id };
const accepted: QuestionDraft = {
  ids, anchor: { head: { branch, version: 1 }, source }, owner: crypto.randomUUID(), edit: 4,
  text: "New draft", receipt: { source, launch: temporary, accepted: temporary, message_id: crypto.randomUUID() },
  pending: { id: crypto.randomUUID(), edit: 3, observed: temporary, user: { text: "Original question" } },
};

function store() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
    removeItem: (key: string) => { values.delete(key); },
    values,
  };
}

test("restores one verified scope and separates the current draft from the original request", () => {
  const storage = store();
  expect(new QuestionDrafts(storage).save(binding, accepted)).toBe("saved");
  const reopened = new QuestionDrafts(storage);
  expect(reopened.get(binding)).toEqual(accepted);
  expect(reopened.get({ ...binding, scope: crypto.randomUUID() })).toBeUndefined();
  expect(reopened.get({ ...binding, branch: crypto.randomUUID() })).toBeUndefined();
  reopened.remove(binding);
  expect(new QuestionDrafts(storage).get(binding)).toBeUndefined();
});

test("falling back to an operation identity leaves an oversized send lookup-only", () => {
  const storage = store();
  const oversized = { ...accepted, pending: { ...accepted.pending!, user: { text: "问".repeat(70_000) } } };
  expect(new QuestionDrafts(storage).save(binding, oversized)).toBe("partial");
  const pending = new QuestionDrafts(storage).get(binding)?.pending;
  expect(pending).toEqual({ id: accepted.pending!.id, observed: temporary, edit: 3 });
});

test("refuses an archived pending position from another temporary owner", () => {
  const storage = store();
  const wrong = { ...accepted, pending: { ...accepted.pending!, observed: { ...temporary, host_generation: crypto.randomUUID() } } };
  new QuestionDrafts(storage).save(binding, wrong);
  expect(new QuestionDrafts(storage).get(binding)).toBeUndefined();
});

test("a saved question receipt restores only for the original temporary boundary and durable Flow", () => {
  const storage = store();
  const { pending: _pending, ...question } = accepted;
  const publication = {
    source: temporary,
    destination: { journal_id: ids.question_id, sequence: temporary.sequence, digest: "c".repeat(64) },
    flow: { session_id: flow.session_id, instance_id: ids.question_id },
    outputs_digest: `blake3:${"d".repeat(64)}`,
  };
  const keep = { keep_request_id: crypto.randomUUID(), expected: temporary, publication };
  new QuestionDrafts(storage).save(binding, { ...question, keep });
  expect(new QuestionDrafts(storage).get(binding)?.keep).toEqual(keep);

  for (const foreign of [
    { ...publication, source: { ...temporary, digest: "e".repeat(64) } },
    { ...publication, destination: { ...publication.destination, journal_id: crypto.randomUUID() } },
  ]) {
    new QuestionDrafts(storage).save(binding, { ...question, keep: { ...keep, publication: foreign } });
    expect(new QuestionDrafts(storage).get(binding)).toBeUndefined();
  }
});

test("storage pressure preserves prior accepted question identities", () => {
  const storage = store();
  const archive = new QuestionDrafts(storage);
  expect(archive.save(binding, accepted)).toBe("saved");
  const created: DraftBinding[] = [];
  for (let i = 0; i < 8; i++) {
    const branch = crypto.randomUUID();
    const next = { ...binding, branch };
    const source = { ...accepted.anchor.source, branch };
    expect(archive.save(next, { ...accepted, anchor: { head: { branch, version: 2 }, source }, receipt: { ...accepted.receipt!, source },
      text: "x".repeat(60_000) })).toBe("saved");
    created.push(next);
  }
  const reopened = new QuestionDrafts(storage);
  for (const original of [binding, ...created]) expect(reopened.get(original)?.receipt?.launch).toEqual(temporary);
});
