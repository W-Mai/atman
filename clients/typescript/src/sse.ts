import { cancelled, ClientError } from "./errors.ts";

export interface SseEvent {
  readonly event: string;
  readonly data: string;
  readonly id: string;
}

const DEFAULT_EVENT_BYTES = 16 * 1024 * 1024;

/** Decode complete SSE events; EOF does not commit an unterminated event. */
export async function* parseSse(
  body: ReadableStream<Uint8Array>,
  options: { readonly signal?: AbortSignal; readonly maxEventBytes?: number } = {},
): AsyncGenerator<SseEvent> {
  const maximum = options.maxEventBytes ?? DEFAULT_EVENT_BYTES;
  if (!Number.isSafeInteger(maximum) || maximum < 1) {
    throw new ClientError("request", "Event byte budget must be a positive safe integer");
  }
  if (options.signal?.aborted) {
    await body.cancel();
    throw cancelled();
  }
  const reader = body.getReader();
  const stop = () => { void reader.cancel().catch(() => {}); };
  options.signal?.addEventListener("abort", stop, { once: true });
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  let buffer = new Uint8Array(Math.min(1024, maximum));
  let length = 0;
  let pendingBytes = 0;
  let skipLf = false;
  let firstLine = true;
  let data: string[] = [];
  let eventType = "";
  let lastId = "";
  let finished = false;
  try {
    while (true) {
      if (options.signal?.aborted) throw cancelled();
      const next = await reader.read();
      if (options.signal?.aborted) throw cancelled();
      if (next.done) {
        finished = true;
        return;
      }
      for (const byte of next.value) {
        pendingBytes += 1;
        if (pendingBytes > maximum) {
          throw new ClientError("limit", "Pending event exceeds its byte budget");
        }
        if (skipLf) {
          skipLf = false;
          if (byte === 10) continue;
        }
        if (byte !== 10 && byte !== 13) {
          if (length === buffer.length) {
            const grown = new Uint8Array(Math.min(maximum, Math.max(1, length * 2)));
            grown.set(buffer);
            buffer = grown;
          }
          buffer[length++] = byte;
          continue;
        }
        skipLf = byte === 13;
        let line: string;
        try {
          line = decoder.decode(buffer.subarray(0, length));
        } catch {
          throw new ClientError("protocol", "Event stream contains invalid UTF-8");
        }
        length = 0;
        if (firstLine) {
          firstLine = false;
          if (line.startsWith("\uFEFF")) line = line.slice(1);
        }
        if (line === "") {
          const event = data.length === 0
            ? undefined
            : { event: eventType || "message", data: data.join("\n"), id: lastId };
          data = [];
          eventType = "";
          pendingBytes = 0;
          if (event !== undefined) yield event;
          continue;
        }
        if (line.startsWith(":")) continue;
        const colon = line.indexOf(":");
        const field = colon < 0 ? line : line.slice(0, colon);
        let value = colon < 0 ? "" : line.slice(colon + 1);
        if (value.startsWith(" ")) value = value.slice(1);
        if (field === "data") data.push(value);
        else if (field === "event") eventType = value;
        else if (field === "id" && !value.includes("\0")) lastId = value;
      }
    }
  } finally {
    options.signal?.removeEventListener("abort", stop);
    if (!finished) await reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}
