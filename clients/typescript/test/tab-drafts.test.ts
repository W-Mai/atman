import { expect, test } from "bun:test";
import type { Dtos, FlowSource } from "../src/index.ts";
import { bindDraft, TabDrafts } from "../web/tab-drafts.ts";

const storageKey = "atman.web.drafts.v1";
const id = (n: number): string => "aaaaaaaa-aaaa-4aaa-8aaa-" + n.toString(16).padStart(12, "0");
const scope = id(1);
const flow = { session_id: id(2), instance_id: id(3) };
const branch = id(4);
const request = id(5);

const page = (source: FlowSource = flow, selected = branch): Dtos["ConversationPage"] => ({
  flow: { ...source }, head: { branch: selected, version: 8 },
  source: { flow: { ...source }, branch: selected,
    cursor: { journal_id: source.instance_id, sequence: 9, digest: "a".repeat(64) } },
  items: [], trailing_bytes: 0,
});

function memory() {
  const raw = new Map<string, string>();
  return {
    raw,
    storage: {
      getItem: (key: string) => raw.get(key) ?? null,
      setItem: (key: string, value: string) => { raw.set(key, value); },
      removeItem: (key: string) => { raw.delete(key); },
    },
  };
}

test("a reload keeps that branch and profile's complete draft and frozen pending request", () => {
  const { storage, raw } = memory();
  const left = bindDraft(scope, flow, page())!;
  const right = bindDraft(scope, flow, page(flow, id(6)))!;
  const secondProfile = bindDraft(id(20), flow, page())!;
  const sameFlowAnotherSession = { ...flow, session_id: id(21) };
  const anotherSession = bindDraft(scope, sameFlowAnotherSession, page(sameFlowAnotherSession))!;
  const snapshot = {
    text: "new edits", edit: 5,
    pending: { request_id: request, head: { branch, version: 7 }, edit: 3, text: "原文\r\nsame request" },
  };

  const writes = new TabDrafts(storage);
  expect(writes.save(left, snapshot)).toBe("saved");
  expect(writes.save(right, { text: "branch B", edit: 2 })).toBe("saved");
  expect(writes.save(secondProfile, { text: "profile B", edit: 1 })).toBe("saved");
  expect(writes.save(anotherSession, { text: "another Session", edit: 1 })).toBe("saved");

  const recovered = new TabDrafts(storage);
  expect(recovered.get(left)).toEqual(snapshot);
  expect(recovered.get(right)?.text).toBe("branch B");
  expect(recovered.get(secondProfile)?.text).toBe("profile B");
  expect(recovered.get(anotherSession)?.text).toBe("another Session");
  expect(recovered.get(bindDraft(id(30), flow, page())!)).toBeUndefined();
  expect(raw.size).toBe(1);
});

test("a fresh draft binding requires both original addresses, branch, and full receipt", () => {
  const canonical = page();
  expect(bindDraft(scope, flow, canonical)).toBeDefined();
  expect(bindDraft(scope, { ...flow, session_id: id(6) }, canonical)).toBeUndefined();
  expect(bindDraft(scope, flow, { ...canonical, source: { ...canonical.source, branch: id(6) } })).toBeUndefined();
  expect(bindDraft(scope, flow, {
    ...canonical, source: { ...canonical.source, flow: { ...flow, instance_id: id(6) } },
  })).toBeUndefined();
  expect(bindDraft(scope, flow, {
    ...canonical, source: { ...canonical.source, cursor: { ...canonical.source.cursor, journal_id: id(6) } },
  })).toBeUndefined();
  expect(bindDraft(scope, flow, {
    ...canonical, source: { ...canonical.source, cursor: { ...canonical.source.cursor, digest: "corrupt" } },
  })).toBeUndefined();
});

test("oversized text restores only the pending identity and never a shortened replay", () => {
  const { storage, raw } = memory();
  const binding = bindDraft(scope, flow, page())!;
  const text = "汉".repeat(40_000);
  const pending = { request_id: request, head: { branch, version: 6 }, edit: 3, text };
  expect(new TabDrafts(storage).save(binding, { text, edit: 3, pending })).toBe("partial");
  expect(new TabDrafts(storage).get(binding)).toEqual({
    edit: 3, pending: { request_id: request, head: { branch, version: 6 }, edit: 3 },
  });
  expect(raw.get(storageKey)?.length).toBeLessThan(1024);
});

test("incomplete or unsupported pending payload stays lookup-only or is rejected", () => {
  const { storage, raw } = memory();
  const binding = bindDraft(scope, flow, page())!;
  const key = [scope, flow.session_id, flow.instance_id, flow.instance_id, branch].join("/");
  const pending = { request_id: request, head: { branch, version: 6 }, edit: 1, text: "first" };
  const input = (payload: unknown) =>
    raw.set(storageKey, JSON.stringify({ version: 1, drafts: [[key, { edit: 1, text: "new", pending: payload }]] }));

  input({ ...pending, unknown_attachment: { image: "omitted" } });
  expect(new TabDrafts(storage).get(binding)?.pending).toEqual({
    request_id: request, head: { branch, version: 6 }, edit: 1,
  });
  input({ ...pending, text: 123 });
  expect(new TabDrafts(storage).get(binding)?.pending?.text).toBeUndefined();
  input({ ...pending, head: { branch: id(8), version: 6 } });
  expect(new TabDrafts(storage).get(binding)).toBeUndefined();
  input({ ...pending, request_id: "missing" });
  expect(new TabDrafts(storage).get(binding)).toBeUndefined();
});

test("over-budget, versioned, or unavailable storage leaves the editor working in memory", () => {
  const { storage, raw } = memory();
  const binding = bindDraft(scope, flow, page())!;
  for (const archived of ["è".repeat(140_000), "{unclosed", '{"version":2,"drafts":[]}']) {
    raw.set(storageKey, archived);
    const vault = new TabDrafts(storage);
    expect(vault.get(binding)).toBeUndefined();
    expect(vault.save(binding, { text: "owned by editor", edit: 1 })).toBe("memory");
    expect(raw.get(storageKey)).toBe(archived);
  }
  const blocked = new TabDrafts({
    ...storage,
    getItem() { throw new Error("Permission denied"); },
  });
  expect(blocked.save(binding, { text: "working", edit: 2 })).toBe("memory");
  expect(blocked.get(binding)).toBeUndefined();
});

test("the aggregate budget keeps pending IDs and the newest request while reducing old payloads", () => {
  const { storage, raw } = memory();
  const drafts = new TabDrafts(storage);
  const items = Array.from({ length: 9 }, (_, index) => {
    const selected = id(100 + index);
    return {
      bound: bindDraft(scope, flow, page(flow, selected))!,
      pending: {
        request_id: id(300 + index), head: { branch: selected, version: 2 },
        text: "x".repeat(40_000), edit: 0,
      },
    };
  });
  for (const { bound, pending } of items)
    expect(drafts.save(bound, { text: pending.text, edit: 0, pending })).toBe("saved");
  const recovered = new TabDrafts(storage);
  expect(new TextEncoder().encode(raw.get(storageKey)).length).toBeLessThanOrEqual(256 * 1024);
  for (const { bound, pending } of items)
    expect(recovered.get(bound)?.pending?.request_id).toBe(pending.request_id);
  expect(recovered.get(items[0]!.bound)?.pending?.text).toBeUndefined();
  expect(recovered.get(items.at(-1)!.bound)?.pending?.text).toBe("x".repeat(40_000));
});

test("a quota error falls back to the complete lookup identity when that fits", () => {
  const { storage } = memory();
  const limited = { ...storage,
    setItem(key: string, value: string) {
      if (value.length > 1024) throw new DOMException("Fixture quota", "QuotaExceededError");
      storage.setItem(key, value);
    },
  };
  const binding = bindDraft(scope, flow, page())!;
  const text = "x".repeat(4000);
  const pending = { request_id: request, head: { branch, version: 6 }, edit: 3, text };
  expect(new TabDrafts(limited).save(binding, { text, edit: 3, pending })).toBe("partial");
  expect(new TabDrafts(limited).get(binding)).toEqual({
    edit: 3, pending: { request_id: request, head: { branch, version: 6 }, edit: 3 },
  });
});

test("removing a settled branch never erases another branch or credential binding", () => {
  const { storage, raw } = memory();
  const input = { text: "bound draft", edit: 1 };
  const a = bindDraft(scope, flow, page())!;
  const otherBranch = bindDraft(scope, flow, page(flow, id(7)))!;
  const otherScope = bindDraft(id(10), flow, page())!;
  const drafts = new TabDrafts(storage);
  for (const key of [a, otherBranch, otherScope]) expect(drafts.save(key, input)).toBe("saved");

  drafts.remove(a);
  const reloaded = new TabDrafts(storage);
  expect(reloaded.get(a)).toBeUndefined();
  expect(reloaded.get(otherBranch)).toEqual(input);
  expect(reloaded.get(otherScope)).toEqual(input);
  expect(reloaded.save(otherBranch, { text: "", edit: 1 })).toBe("saved");
  reloaded.remove(otherScope);
  expect(raw.has(storageKey)).toBe(false);
});
