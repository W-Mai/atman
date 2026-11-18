import { expect, test } from "bun:test";
import { unlink } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { createSchemaDecoder } from "../src/schema.ts";
import type { SchemaRule } from "../src/schema.ts";
import type { RpcMethod } from "../src/client.ts";
import { generate } from "../tools/generate.ts";

function bundle(schemas: Record<string, unknown>) {
  return {
    openapi: { openapi: "3.1.0", components: { schemas } },
    capabilities: {
      protocol_revision: 1, schema_revision: 1,
      methods: { "fixture.read": { revision: 2, request_schema: "Request", response_schema: "Response" } },
    },
    operations: { "fixture.read": "query" },
  };
}

test("generated descriptors compile and reject malformed wire values before DTO installation", async () => {
  const fixture = bundle({
    Request: {
      type: "object", required: ["id"], additionalProperties: false,
      properties: { id: { type: "string", format: "uuid" }, enabled: { type: "boolean" }, keyed: { $ref: "#/components/schemas/Keyed" } },
    },
    Response: {
      type: "object", required: ["root", "position"], additionalProperties: false,
      properties: {
        root: { $ref: "#/components/schemas/Node" },
        position: { type: "integer", minimum: 0, format: "int64" },
        note: { type: ["string", "null"] },
        payload: {},
        keyed: { $ref: "#/components/schemas/Keyed" },
      },
    },
    Node: {
      type: "object", required: ["value"], additionalProperties: false,
      properties: { value: { type: "string" }, children: { type: "array", items: { $ref: "#/components/schemas/Node" } } },
    },
    KeyName: { type: "string", format: "uuid" },
    Keyed: { type: "object", properties: { broken: { type: "integer" } },
      additionalProperties: true, propertyNames: { $ref: "#/components/schemas/KeyName" } },
  });
  const generated = generate(fixture);
  expect(generated.methods).toEqual(["fixture.read"]);
  expect(generated.skipped).toEqual({});
  const path = fileURLToPath(new URL(`../src/.schema-check-${crypto.randomUUID()}.ts`, import.meta.url));
  try {
    await Bun.write(path, generated.source + `
// @ts-expect-error Required IDs are not nullable.
const invalidId: Dtos["Request"] = { id: null }; void invalidId;
// @ts-expect-error Optional fields do not implicitly accept null.
const invalidOptional: Dtos["Request"] = { id: "text", enabled: null }; void invalidOptional;
// @ts-expect-error Closed request DTOs have no undeclared fields.
const invalidExtra: Dtos["Request"] = { id: "text", extra: true }; void invalidExtra;
// @ts-expect-error Opaque JSON does not include functions.
const invalidOpaque: Dtos["Response"] = { root: { value: "x" }, position: 1, payload: () => 0 }; void invalidOpaque;
`);
    const check = Bun.spawn([process.execPath, "run", "check"], {
      cwd: fileURLToPath(new URL("..", import.meta.url)), stdout: "pipe", stderr: "pipe",
    });
    const [status, stdout, stderr] = await Promise.all([check.exited, new Response(check.stdout).text(), new Response(check.stderr).text()]);
    expect({ status, stdout, stderr: stderr.replace("$ tsc --noEmit\n", "") }).toEqual({ status: 0, stdout: "", stderr: "" });
    const imported: { methods: Record<string, RpcMethod<unknown, unknown>> } = await import(path);
    const method = imported.methods["fixture.read"]!;
    expect(method.revision).toBe(2);
    expect(method.operation).toBe("query");
    expect(() => method.validate?.({})).toThrow();
    expect(() => method.validate?.({ id: null })).toThrow();
    expect(() => method.validate?.({ id: "not-a-uuid" })).toThrow();
    expect(() => method.validate?.({ id: crypto.randomUUID(), extra: true })).toThrow();
    expect(() => method.validate?.({ id: crypto.randomUUID(), keyed: { broken: 1 } })).toThrow();
    expect(() => method.validate?.({ id: crypto.randomUUID(), keyed: { extra: 1 } })).toThrow();
    expect(() => method.validate?.({ id: crypto.randomUUID(), keyed: { [crypto.randomUUID()]: 1 } })).not.toThrow();
    expect(() => method.validate?.({ id: crypto.randomUUID() })).not.toThrow();
    const valid = { root: { value: "root", children: [{ value: "child" }] }, position: 1, note: null,
      payload: { arbitrary: [1, "x"] }, keyed: { [crypto.randomUUID()]: 1 } };
    expect(method.decode(valid)).toBe(valid);
    for (const malformed of [
      null, {}, { ...valid, root: { value: 1 } }, { ...valid, note: 1 },
      { ...valid, position: 9_007_199_254_740_992 }, { ...valid, position: -1 },
      { ...valid, payload: () => 0 }, { ...valid, extra: true }, { ...valid, note: undefined },
      { ...valid, keyed: { broken: 1 } }, { ...valid, keyed: { extra: 1 } },
    ]) expect(() => method.decode(malformed)).toThrow();
  } finally {
    await unlink(path).catch(() => {});
  }
});

test("generation omits missing contracts and refuses unsupported or unproductive schema semantics", () => {
  for (const unsupported of [
    { type: "string", not: { const: "x" } },
    { type: "string", format: "custom-format" },
    { $ref: "https://schema.example/External" },
    { $ref: "#/components/schemas/Missing" },
    { $ref: "#/components/schemas/Response" },
    { type: [] }, { type: "array", items: null },
    { type: "object", additionalProperties: null },
    { type: "object", propertyNames: { not: { const: "x" } } },
    { properties: { field: { type: "string" } } },
    { type: "object", properties: { field: { type: "string" } }, additionalProperties: { type: "integer" } },
  ]) {
    const result = generate(bundle({ Request: {}, Response: unsupported }));
    expect(result.methods).toEqual([]);
    expect(result.skipped["fixture.read"]).toBeString();
    expect(result.source).not.toContain('"fixture.read": defineMethod');
  }
  const input = bundle({ Request: {}, Response: {} });
  const result = generate({ ...input, capabilities: { ...input.capabilities, methods: {
    legacy: { revision: 1, request_schema: "Request" }, ...input.capabilities.methods,
  } } });
  expect(result.methods).toEqual(["fixture.read"]);
  expect(result.skipped.legacy).toContain("complete request/response");
  const noMode = generate({ ...input, operations: {} });
  expect(noMode.methods).toEqual([]);
  expect(noMode.skipped["fixture.read"]).toContain("operation mode");
});

test("property-name constraints apply to every own key through the original rule and reference graph", () => {
  const rules: Record<string, SchemaRule> = {
    Name: { kind: "string", pattern: "^[a-z]+$" },
    Names: { kind: "object", properties: { BROKEN: { kind: "integer" } }, required: [], additional: true,
      propertyNames: { kind: "ref", name: "Name" } },
    Empty: { kind: "object", properties: {}, required: [], additional: true, propertyNames: { kind: "never" } },
  };
  const decode = createSchemaDecoder<Record<string, unknown>>(rules);
  rules.Name = { kind: "never" };
  expect(decode("Names", { valid: 1 })).toEqual({ valid: 1 });
  expect(() => decode("Names", { BROKEN: 1 })).toThrow();
  expect(() => decode("Names", { invalid1: 1 })).toThrow();
  expect(decode("Empty", {})).toEqual({});
  expect(() => decode("Empty", { any: 1 })).toThrow();
  const limited = createSchemaDecoder<Record<string, unknown>>(rules, { maxSteps: 2 });
  expect(() => limited("Names", { valid: 1 })).toThrow("preparation budget");
  const recursive = generate(bundle({ Request: {}, Response: {
    type: "object", propertyNames: { $ref: "#/components/schemas/Response" }, additionalProperties: true,
  } }));
  expect(recursive.methods).toEqual(["fixture.read"]);
  const inline = generate(bundle({
    Request: { type: "object", additionalProperties: false, propertyNames: {} },
    Response: { type: "object", additionalProperties: false, propertyNames: false },
  }));
  expect(inline.methods).toEqual(["fixture.read"]);
  expect(inline.source).not.toContain("import type { JsonValue,");
});

test("schema rules distinguish null, optional, unions, closed properties and original JSON data", () => {
  const schemas: Record<string, SchemaRule> = {
    Closed: { kind: "object", properties: {
      id: { kind: "string" }, note: { kind: "any", rules: [{ kind: "string" }, { kind: "null" }] },
    }, required: ["id"], additional: false },
    Open: { kind: "object", properties: {}, required: [], additional: { kind: "integer" } },
    ExactlyOne: { kind: "one", rules: [{ kind: "string" }, { kind: "enum", values: ["overlap"] }] },
    Both: { kind: "all", rules: [{ kind: "integer", minimum: 0 }, { kind: "integer", exclusiveMaximum: 3 }] },
    Opaque: { kind: "json" },
    Character: { kind: "string", minLength: 1, maxLength: 1 },
    Date: { kind: "string", format: "date" },
    Timestamp: { kind: "string", format: "date-time" },
  };
  const decode = createSchemaDecoder<Record<string, unknown>>(schemas);
  expect(decode("Closed", { id: "x" })).toEqual({ id: "x" });
  expect(decode("Closed", { id: "x", note: null })).toEqual({ id: "x", note: null });
  for (const value of [null, {}, { id: "x", extra: true }, { id: "x", note: undefined }]) {
    expect(() => decode("Closed", value)).toThrow();
  }
  expect(() => decode("Closed", JSON.parse('{"id":"x","__proto__":{}}'))).toThrow();
  expect(decode("Open", { a: 1, b: 2 })).toEqual({ a: 1, b: 2 });
  expect(() => decode("Open", { a: "1" })).toThrow();
  expect(decode("ExactlyOne", "different")).toBe("different");
  expect(() => decode("ExactlyOne", "overlap")).toThrow();
  expect(decode("Both", 2)).toBe(2);
  for (const number of [-1, 0.5, 3, Infinity, NaN]) expect(() => decode("Both", number)).toThrow();
  expect(decode("Character", "🙂")).toBe("🙂");
  expect(() => decode("Character", "🙂x")).toThrow();
  for (const value of [new Date(), new Map(), undefined, { nested: Infinity }]) expect(() => decode("Opaque", value)).toThrow();
  expect(decode("Date", "2024-02-29")).toBe("2024-02-29");
  for (const value of ["2023-02-29", "2024-02-30", "2024-13-01"]) expect(() => decode("Date", value)).toThrow();
  expect(decode("Timestamp", "2024-02-29T23:59:60Z")).toBeString();
  for (const value of ["2024-02-30T00:00:00Z", "2024-02-29T24:00:00Z", "2024-02-29T12:00:00+24:00"]) {
    expect(() => decode("Timestamp", value)).toThrow();
  }
});

test("recursive decoders retain captured rules and enforce independent depth and work budgets", () => {
  const rules: Record<string, SchemaRule> = {
    Root: { kind: "object", properties: { next: { kind: "ref", name: "Root" } }, required: [], additional: false },
  };
  const decode = createSchemaDecoder<{ Root: unknown }>(rules, { maxDepth: 8 });
  rules.Root = { kind: "never" };
  expect(decode("Root", { next: {} })).toEqual({ next: {} });
  const cycle: { next?: unknown } = {};
  cycle.next = cycle;
  expect(() => decode("Root", cycle)).toThrow("preparation budget");
  const constrained = createSchemaDecoder<{ Json: unknown }>({ Json: { kind: "json" } }, { maxSteps: 4 });
  expect(() => constrained("Json", [1, 2, 3, 4])).toThrow("preparation budget");
  expect(() => createSchemaDecoder({}, { maxDepth: 0 })).toThrow();
});
