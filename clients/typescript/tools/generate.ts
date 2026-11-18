import type { SchemaRule } from "../src/schema.ts";

type JsonObject = Record<string, unknown>;
type Operation = "query" | "command";

export interface Generation {
  readonly source: string;
  readonly methods: readonly string[];
  readonly skipped: Readonly<Record<string, string>>;
}

const annotations = new Set([
  "title", "description", "default", "deprecated", "example", "examples",
  "readOnly", "writeOnly", "discriminator", "externalDocs", "xml",
]);
const keywords = new Set([
  "$ref", "type", "enum", "const", "allOf", "anyOf", "oneOf", "properties", "required",
  "additionalProperties", "propertyNames", "items", "minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum",
  "minLength", "maxLength", "pattern", "format", "minItems", "maxItems", "minProperties", "maxProperties",
]);

function object(value: unknown, label: string): JsonObject {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error(`${label}: expected an object`);
  return value as JsonObject;
}

function positive(value: unknown, label: string): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1 || value > 0xffff_ffff) {
    throw new Error(`${label}: expected a positive revision`);
  }
  return value;
}

function reference(value: unknown): string {
  const prefix = "#/components/schemas/";
  if (typeof value !== "string" || !value.startsWith(prefix)) throw new Error("Only local component schema references are supported");
  const name = value.slice(prefix.length);
  if (name.includes("/") || /~(?![01])/.test(name)) throw new Error("Schema reference has an invalid JSON pointer");
  return name.replace(/~1/g, "/").replace(/~0/g, "~");
}

function bound(source: JsonObject, key: string, integer = false): number | undefined {
  const value = source[key];
  if (value === undefined) return undefined;
  if (typeof value !== "number" || !Number.isFinite(value)
    || (integer && (!Number.isSafeInteger(value) || value < 0))) throw new Error(`Invalid schema ${key}`);
  return value;
}

function limits(source: JsonObject, keys: readonly string[], integer = false): Record<string, number> {
  return Object.fromEntries(keys.flatMap((key) => {
    const value = bound(source, key, integer);
    return value === undefined ? [] : [[key, value]];
  }));
}

function primitive(value: unknown): value is null | boolean | number | string {
  return value === null || typeof value === "boolean" || typeof value === "string"
    || typeof value === "number" && Number.isFinite(value);
}

function compile(value: unknown): SchemaRule {
  if (typeof value === "boolean") return { kind: value ? "json" : "never" };
  const schema = object(value, "schema");
  for (const key of Object.keys(schema)) {
    if (!keywords.has(key) && !annotations.has(key)) throw new Error(`Unsupported schema keyword ${key}`);
  }
  const rules: SchemaRule[] = [];
  if (schema.$ref !== undefined) rules.push({ kind: "ref", name: reference(schema.$ref) });
  for (const [key, kind] of [["allOf", "all"], ["anyOf", "any"], ["oneOf", "one"]] as const) {
    if (schema[key] === undefined) continue;
    if (!Array.isArray(schema[key])) throw new Error(`${key} must be an array`);
    const children = schema[key].map(compile);
    rules.push(children.length === 0 && kind === "all" ? { kind: "json" } : { kind, rules: children });
  }
  if (schema.enum !== undefined) {
    if (!Array.isArray(schema.enum) || schema.enum.length === 0 || !schema.enum.every(primitive)) {
      throw new Error("Only nonempty primitive enum values are supported");
    }
    rules.push({ kind: "enum", values: schema.enum });
  }
  if (Object.hasOwn(schema, "const")) {
    if (!primitive(schema.const)) throw new Error("Only primitive const values are supported");
    rules.push({ kind: "enum", values: [schema.const] });
  }
  const types = schema.type === undefined ? [] : Array.isArray(schema.type) ? schema.type : [schema.type];
  if (schema.type !== undefined && types.length === 0
    || !types.every((type): type is string => typeof type === "string") || new Set(types).size !== types.length) {
    throw new Error("Invalid schema type list");
  }
  const constraints: readonly [readonly string[], string][] = [
    [["properties", "required", "additionalProperties", "propertyNames", "minProperties", "maxProperties"], "object"],
    [["items", "minItems", "maxItems"], "array"],
    [["minLength", "maxLength", "pattern"], "string"],
    [["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"], "number"],
  ];
  for (const [keys, type] of constraints) {
    if (keys.some((key) => schema[key] !== undefined) && !types.includes(type)
      && !(type === "number" && types.includes("integer"))) throw new Error(`Schema constraints require explicit ${type} type`);
  }
  if (schema.format !== undefined && !types.includes("string") && !types.includes("number") && !types.includes("integer")) {
    throw new Error("Schema format requires an explicit string or number type");
  }
  const typed = types.map((type): SchemaRule => {
    switch (type) {
      case "null": case "boolean": return { kind: type };
      case "integer": case "number": {
        if (schema.format !== undefined && !["int32", "int64", "uint32", "uint64", "float", "double"].includes(String(schema.format))) {
          throw new Error(`Unsupported numeric format ${String(schema.format)}`);
        }
        return { kind: type, ...limits(schema, ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]) };
      }
      case "string": {
        const result: Extract<SchemaRule, { kind: "string" }> = { kind: "string", ...limits(schema, ["minLength", "maxLength"], true) };
        const pattern = schema.pattern;
        if (pattern !== undefined) {
          if (typeof pattern !== "string") throw new Error("Invalid string pattern");
          new RegExp(pattern, "u");
          Object.assign(result, { pattern });
        }
        const format = schema.format;
        if (format !== undefined) {
          if (format !== "uuid" && format !== "date-time" && format !== "date" && format !== "byte") {
            throw new Error(`Unsupported string format ${String(format)}`);
          }
          Object.assign(result, { format });
        }
        return result;
      }
      case "array": return { kind: "array", items: compile(schema.items === undefined ? true : schema.items), ...limits(schema, ["minItems", "maxItems"], true) };
      case "object": {
        const properties = Object.fromEntries(Object.entries(schema.properties === undefined ? {} : object(schema.properties, "properties"))
          .map(([key, property]) => [key, compile(property)]));
        const required: unknown = schema.required === undefined ? [] : schema.required;
        if (!Array.isArray(required) || !required.every((key): key is string => typeof key === "string")
          || new Set(required).size !== required.length) throw new Error("Invalid required property list");
        if (required.some((key) => !Object.hasOwn(properties, key))) throw new Error("Required property needs an explicit schema");
        const additional = schema.additionalProperties === undefined ? true : schema.additionalProperties;
        if (typeof additional !== "boolean" && Object.keys(properties).length > 0) {
          throw new Error("Schema-valued additional properties with named properties need an explicit DTO shape");
        }
        return { kind: "object", properties, required, additional: typeof additional === "boolean" ? additional : compile(additional),
          ...(schema.propertyNames === undefined ? {} : { propertyNames: compile(schema.propertyNames) }),
          ...limits(schema, ["minProperties", "maxProperties"], true) };
      }
      default: throw new Error(`Unsupported schema type ${type}`);
    }
  });
  if (typed.length > 0) rules.push(typed.length === 1 ? typed[0]! : { kind: "any", rules: typed });
  return rules.length === 0 ? { kind: "json" } : rules.length === 1 ? rules[0]! : { kind: "all", rules };
}

function references(rule: SchemaRule, guarded = false): { name: string; guarded: boolean }[] {
  switch (rule.kind) {
    case "ref": return [{ name: rule.name, guarded }];
    case "all": case "any": case "one": return rule.rules.flatMap((child) => references(child, guarded));
    case "array": return references(rule.items, true);
    case "object": return [...Object.values(rule.properties), ...(typeof rule.additional === "boolean" ? [] : [rule.additional]),
      ...(rule.propertyNames === undefined ? [] : [rule.propertyNames])]
      .flatMap((child) => references(child, true));
    default: return [];
  }
}

function collect(root: string, components: JsonObject): Record<string, SchemaRule> {
  const rules: Record<string, SchemaRule> = Object.create(null) as Record<string, SchemaRule>;
  const pending = [root];
  while (pending.length > 0) {
    const name = pending.pop()!;
    if (Object.hasOwn(rules, name)) continue;
    if (!Object.hasOwn(components, name)) throw new Error(`Missing component schema ${name}`);
    const rule = compile(components[name]);
    rules[name] = rule;
    pending.push(...references(rule).map((reference) => reference.name));
  }
  const active = new Set<string>();
  const completed = new Set<string>();
  function visit(name: string): void {
    if (active.has(name)) throw new Error(`Unguarded recursive schema ${name}`);
    if (completed.has(name)) return;
    active.add(name);
    for (const ref of references(rules[name]!)) if (!ref.guarded) visit(ref.name);
    active.delete(name);
    completed.add(name);
  }
  for (const name of Object.keys(rules)) visit(name);
  return rules;
}

function typeOf(rule: SchemaRule): string {
  switch (rule.kind) {
    case "json": return "JsonValue";
    case "never": return "never";
    case "null": case "boolean": case "number": case "string": return rule.kind;
    case "integer": return "number";
    case "ref": return `Dtos[${JSON.stringify(rule.name)}]`;
    case "enum": return rule.values.map((value) => JSON.stringify(value)).join(" | ") || "never";
    case "all": return rule.rules.map((child) => `(${typeOf(child)})`).join(" & ") || "JsonValue";
    case "any": case "one": return rule.rules.map((child) => `(${typeOf(child)})`).join(" | ") || "never";
    case "array": return `ReadonlyArray<${typeOf(rule.items)}>`;
    case "object": {
      const properties = Object.entries(rule.properties).sort(([left], [right]) => left.localeCompare(right)).map(([key, child]) =>
        `readonly ${JSON.stringify(key)}${rule.required.includes(key) ? "" : "?"}: ${typeOf(child)};`);
      const base = `{ ${properties.join(" ")} }`;
      if (rule.additional === false) return properties.length === 0 ? "Readonly<Record<string, never>>" : base;
      const value = rule.additional === true ? "JsonValue" : typeOf(rule.additional);
      const extras = `Readonly<Record<string, ${value}>>`;
      return properties.length === 0 ? extras : `${base} & ${extras}`;
    }
  }
}

function usesJson(rule: SchemaRule): boolean {
  switch (rule.kind) {
    case "json": return true;
    case "all": return rule.rules.length === 0 || rule.rules.some(usesJson);
    case "any": case "one": return rule.rules.some(usesJson);
    case "array": return usesJson(rule.items);
    // Property-name constraints keep string keys; their referenced DTOs are collected separately.
    case "object": return rule.additional === true || Object.values(rule.properties).some(usesJson)
      || typeof rule.additional !== "boolean" && usesJson(rule.additional);
    default: return false;
  }
}

/** Generate only methods whose complete advertised wire schemas can be represented and checked. */
export function generate(bundle: unknown): Generation {
  const input = object(bundle, "export bundle");
  const openapi = object(input.openapi, "openapi");
  if (openapi.openapi !== "3.1.0") throw new Error("Expected the daemon's OpenAPI 3.1.0 document");
  const components = object(object(openapi.components, "components").schemas, "schemas");
  const capabilities = object(input.capabilities, "capabilities");
  const protocol = positive(capabilities.protocol_revision, "protocol_revision");
  const schemaRevision = positive(capabilities.schema_revision, "schema_revision");
  const methods = object(capabilities.methods, "methods");
  const operations = object(input.operations, "exported operation facts");
  const rules: Record<string, SchemaRule> = Object.create(null) as Record<string, SchemaRule>;
  const descriptors: string[] = [];
  const names: string[] = [];
  const skipped: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const name of Object.keys(methods).sort()) {
    try {
      const method = object(methods[name], name);
      const revision = positive(method.revision, `${name}.revision`);
      const request = method.request_schema;
      const response = method.response_schema;
      if (typeof request !== "string" || typeof response !== "string") throw new Error("A complete request/response schema pair is not advertised");
      const operation = operations[name];
      if (operation !== "query" && operation !== "command") throw new Error("An authoritative operation mode is not exported");
      const requestRules = collect(request, components);
      const responseRules = collect(response, components);
      Object.assign(rules, requestRules, responseRules);
      names.push(name);
      descriptors.push(`  ${JSON.stringify(name)}: defineMethod<Dtos[${JSON.stringify(request)}], Dtos[${JSON.stringify(response)}]>({\n`
        + `    name: ${JSON.stringify(name)}, revision: ${revision}, operation: ${JSON.stringify(operation as Operation)},\n`
        + `    request_schema: ${JSON.stringify(request)}, response_schema: ${JSON.stringify(response)},\n`
        + `    validate: (value) => { decode(${JSON.stringify(request)}, value); },\n`
        + `    decode: (value) => decode(${JSON.stringify(response)}, value),\n  }),`);
    } catch (error) {
      skipped[name] = error instanceof Error ? error.message : "Schema generation failed";
    }
  }
  const ordered = Object.fromEntries(Object.keys(rules).sort().map((name) => [name, rules[name]]));
  const declarations = Object.entries(ordered).map(([name, rule]) => `  readonly ${JSON.stringify(name)}: ${typeOf(rule!)};`).join("\n");
  const source = "// Generated from daemon OpenAPI and capability facts. Do not edit.\n"
    + 'import { defineMethod } from "./client.ts";\n'
    + 'import { createSchemaDecoder } from "./schema.ts";\n'
    + `import type { ${Object.values(ordered).some((rule) => usesJson(rule!)) ? "JsonValue, " : ""}SchemaRule } from "./schema.ts";\n\n`
    + `export interface Dtos {\n${declarations}\n}\n\n`
    + `const rules = JSON.parse(${JSON.stringify(JSON.stringify(ordered))}) as Record<string, SchemaRule>;\n`
    + "const decode = createSchemaDecoder<Dtos>(rules);\nexport { decode as decodeDto };\n\n"
    + `export const contract = Object.freeze({ protocol_revision: ${protocol}, schema_revision: ${schemaRevision} });\n`
    + `export const methods = Object.freeze({\n${descriptors.join("\n")}\n});\n`
    + `export const skippedMethods = Object.freeze(${JSON.stringify(skipped, null, 2)});\n`;
  return { source, methods: names, skipped };
}

if (import.meta.main) {
  const [input, output, ...extra] = Bun.argv.slice(2);
  if (input === undefined || output === undefined || extra.length > 0) {
    throw new Error("Usage: bun tools/generate.ts <daemon-export.json> <generated.ts>");
  }
  const generated = generate(await Bun.file(input).json());
  if (generated.methods.length === 0) throw new Error(`No supported methods: ${JSON.stringify(generated.skipped)}`);
  await Bun.write(output, generated.source);
  console.info(JSON.stringify({ methods: generated.methods, skipped: generated.skipped }));
}
