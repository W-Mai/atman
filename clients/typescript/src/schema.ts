import { ClientError } from "./errors.ts";

export type JsonValue = null | boolean | number | string | readonly JsonValue[]
  | { readonly [key: string]: JsonValue };

type Primitive = null | boolean | number | string;

/** Compiled rules preserve the supported OpenAPI schema semantics. */
export type SchemaRule =
  | { readonly kind: "json" | "never" | "null" | "boolean" }
  | { readonly kind: "ref"; readonly name: string }
  | { readonly kind: "enum"; readonly values: readonly Primitive[] }
  | { readonly kind: "all" | "any" | "one"; readonly rules: readonly SchemaRule[] }
  | {
    readonly kind: "number" | "integer";
    readonly minimum?: number;
    readonly maximum?: number;
    readonly exclusiveMinimum?: number;
    readonly exclusiveMaximum?: number;
  }
  | {
    readonly kind: "string";
    readonly minLength?: number;
    readonly maxLength?: number;
    readonly pattern?: string;
    readonly format?: "uuid" | "date-time" | "date" | "byte";
  }
  | {
    readonly kind: "array";
    readonly items: SchemaRule;
    readonly minItems?: number;
    readonly maxItems?: number;
  }
  | {
    readonly kind: "object";
    readonly properties: Readonly<Record<string, SchemaRule>>;
    readonly required: readonly string[];
    readonly additional: boolean | SchemaRule;
    readonly propertyNames?: SchemaRule;
    readonly minProperties?: number;
    readonly maxProperties?: number;
  };

export interface SchemaDecodeLimits {
  readonly maxDepth?: number;
  readonly maxSteps?: number;
}

interface Budget {
  readonly depth: number;
  steps: number;
}

function enter(budget: Budget, depth: number): void {
  if (depth > budget.depth || --budget.steps < 0) {
    throw new ClientError("limit", "Schema decoding exceeds its preparation budget");
  }
}

function object(value: unknown): value is Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return false;
  const prototype: unknown = Object.getPrototypeOf(value);
  return prototype === null || prototype === Object.prototype;
}

function json(value: unknown, budget: Budget, depth: number): boolean {
  enter(budget, depth);
  if (value === null || typeof value === "boolean" || typeof value === "string") return true;
  if (typeof value === "number") return Number.isFinite(value);
  if (Array.isArray(value)) return value.every((entry: unknown) => json(entry, budget, depth + 1));
  return object(value) && Object.values(value).every((entry) => json(entry, budget, depth + 1));
}

function formatted(value: string, format: "uuid" | "date-time" | "date" | "byte"): boolean {
  switch (format) {
    case "uuid": return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value);
    case "byte": return /^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value);
    case "date": return date(value);
    case "date-time": {
      const match = /^(\d{4}-\d{2}-\d{2})[Tt](\d{2}):(\d{2}):(\d{2})(?:\.\d+)?(?:[Zz]|([+-])(\d{2}):(\d{2}))$/.exec(value);
      return match !== null && date(match[1]!) && Number(match[2]) < 24 && Number(match[3]) < 60
        && Number(match[4]) <= 60 && (match[5] === undefined || Number(match[6]) < 24 && Number(match[7]) < 60);
    }
  }
}

function date(value: string): boolean {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  if (match === null) return false;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const leap = year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0);
  const days = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1];
  return days !== undefined && day >= 1 && day <= days;
}

function matches(
  rule: SchemaRule,
  value: unknown,
  schemas: Readonly<Record<string, SchemaRule>>,
  budget: Budget,
  depth: number,
): boolean {
  enter(budget, depth);
  switch (rule.kind) {
    case "json": return json(value, budget, depth);
    case "never": return false;
    case "null": return value === null;
    case "boolean": return typeof value === "boolean";
    case "ref": {
      const target = Object.hasOwn(schemas, rule.name) ? schemas[rule.name] : undefined;
      if (target === undefined) throw new ClientError("protocol", "Schema reference is unavailable");
      return matches(target, value, schemas, budget, depth + 1);
    }
    case "enum": return rule.values.some((entry) => entry === value);
    case "all": return rule.rules.every((entry) => matches(entry, value, schemas, budget, depth + 1));
    case "any": return rule.rules.some((entry) => matches(entry, value, schemas, budget, depth + 1));
    case "one": {
      let count = 0;
      for (const entry of rule.rules) {
        if (matches(entry, value, schemas, budget, depth + 1) && ++count > 1) return false;
      }
      return count === 1;
    }
    case "integer":
    case "number": return typeof value === "number" && Number.isFinite(value)
      && (rule.kind !== "integer" || Number.isSafeInteger(value))
      && (rule.minimum === undefined || value >= rule.minimum)
      && (rule.maximum === undefined || value <= rule.maximum)
      && (rule.exclusiveMinimum === undefined || value > rule.exclusiveMinimum)
      && (rule.exclusiveMaximum === undefined || value < rule.exclusiveMaximum);
    case "string": {
      if (typeof value !== "string") return false;
      let length = 0;
      if (rule.minLength !== undefined || rule.maxLength !== undefined) {
        for (const _character of value) length++;
      }
      return (rule.minLength === undefined || length >= rule.minLength)
        && (rule.maxLength === undefined || length <= rule.maxLength)
        && (rule.pattern === undefined || new RegExp(rule.pattern, "u").test(value))
        && (rule.format === undefined || formatted(value, rule.format));
    }
    case "array": return Array.isArray(value)
      && (rule.minItems === undefined || value.length >= rule.minItems)
      && (rule.maxItems === undefined || value.length <= rule.maxItems)
      && value.every((entry: unknown) => matches(rule.items, entry, schemas, budget, depth + 1));
    case "object": {
      if (!object(value)) return false;
      const keys = Object.keys(value);
      if ((rule.minProperties !== undefined && keys.length < rule.minProperties)
        || (rule.maxProperties !== undefined && keys.length > rule.maxProperties)
        || rule.required.some((key) => !Object.hasOwn(value, key))) return false;
      for (const key of keys) {
        if (rule.propertyNames !== undefined && !matches(rule.propertyNames, key, schemas, budget, depth + 1)) return false;
        if (Object.hasOwn(rule.properties, key)) {
          const property = rule.properties[key];
          if (property === undefined || !matches(property, value[key], schemas, budget, depth + 1)) return false;
        } else if (typeof rule.additional === "boolean") {
          if (!rule.additional || !json(value[key], budget, depth + 1)) return false;
        } else if (!matches(rule.additional, value[key], schemas, budget, depth + 1)) return false;
      }
      return true;
    }
  }
}

/** Capture generated rules once; every response is checked before its DTO cast. */
export function createSchemaDecoder<Models extends object>(
  schemas: Readonly<Record<string, SchemaRule>>,
  limits: SchemaDecodeLimits = {},
): <Name extends Extract<keyof Models, string>>(name: Name, value: unknown) => Models[Name] {
  const captured = structuredClone(schemas);
  const depth = limits.maxDepth ?? 128;
  const steps = limits.maxSteps ?? 1_000_000;
  if (!Number.isSafeInteger(depth) || depth < 1 || !Number.isSafeInteger(steps) || steps < 1) {
    throw new ClientError("request", "Schema decode limits must be positive exact integers", "not_sent");
  }
  return (name, value) => {
    const rule = Object.hasOwn(captured, name) ? captured[name] : undefined;
    if (rule === undefined || !matches(rule, value, captured, { depth, steps }, 0)) {
      throw new ClientError("protocol", `RPC data does not match schema ${name}`);
    }
    return value as Models[typeof name];
  };
}
