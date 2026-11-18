import { ClientError } from "./errors.ts";

/** Structural constraints shared by generated Flow-address types. */
export interface FlowSource {
  readonly session_id: string;
  readonly instance_id: string;
}

/** The durable wire position; no host generation or transient position is implied. */
export interface DurableCursor {
  readonly journal_id: string;
  readonly sequence: number;
  readonly digest: string;
}

export interface DurableBatch {
  readonly receipt: DurableCursor;
}

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const NIL = "00000000-0000-0000-0000-000000000000";

export function canonicalUuid(value: string): string {
  if (typeof value !== "string" || !UUID.test(value) || value.toLowerCase() === NIL) {
    throw new ClientError("source", "Expected a non-nil UUID");
  }
  return value.toLowerCase();
}

export function flowSource(value: FlowSource): Readonly<FlowSource> {
  if (value === null || typeof value !== "object") throw new ClientError("source", "Flow source is absent");
  return Object.freeze({
    session_id: canonicalUuid(value.session_id),
    instance_id: canonicalUuid(value.instance_id),
  });
}

export function durableCursor(value: DurableCursor, source: FlowSource): Readonly<DurableCursor> {
  if (value === null || typeof value !== "object") throw new ClientError("source", "Event receipt is absent");
  const journal = canonicalUuid(value.journal_id);
  if (
    journal !== canonicalUuid(source.instance_id)
    || !Number.isSafeInteger(value.sequence)
    || value.sequence < 1
    || typeof value.digest !== "string"
    || !/^[0-9a-f]{64}$/i.test(value.digest)
  ) {
    throw new ClientError("source", "Event receipt does not belong to its Flow or is not exact");
  }
  return Object.freeze({ journal_id: journal, sequence: value.sequence, digest: value.digest });
}

export function cursorId(value: DurableCursor): string {
  return `${value.journal_id}:${value.sequence}:${value.digest}`;
}

/** A cursor changes only when a complete, source-checked batch is delivered. */
export class EventCursor {
  readonly source: Readonly<FlowSource>;
  #last: Readonly<DurableCursor> | undefined;

  constructor(source: FlowSource, after?: DurableCursor) {
    this.source = flowSource(source);
    this.#last = after === undefined ? undefined : durableCursor(after, this.source);
  }

  get last(): Readonly<DurableCursor> | undefined { return this.#last; }

  deliver(receipt: DurableCursor): boolean {
    const next = durableCursor(receipt, this.source);
    if (this.#last !== undefined) {
      if (next.sequence < this.#last.sequence) return false;
      if (next.sequence === this.#last.sequence) {
        if (next.digest !== this.#last.digest) {
          throw new ClientError("source", "Event sequence has conflicting receipt digests");
        }
        return false;
      }
    }
    this.#last = next;
    return true;
  }
}
