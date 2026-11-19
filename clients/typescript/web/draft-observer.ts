import { ClientError, RemoteError, canonicalUuid, durableCursor, flowSource } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient, FlowSource } from "../src/index.ts";

type Snapshot = Dtos["FlowDraftSnapshot"];
type Edition = Dtos["FlowDraftEdition"];
type Boundary = Pick<Dtos["ConversationPage"], "flow" | "source" | "head">;
type Wait = (delay: number, signal: AbortSignal) => Promise<boolean>;
type Stop = "authentication" | "authorization" | "contract" | "source" | "unavailable";
type Phase = "connected" | "reconnecting" | "detached";

export type DraftObservationState =
  | { readonly state: "ready"; readonly snapshot: Snapshot; readonly reset: boolean }
  | { readonly state: "connected" | "reconnecting" | "detached" | "stale_head" | "head_reset" }
  | { readonly state: "stopped"; readonly reason: Stop };

type Options = {
  readonly flow: Pick<FlowClient, "address" | "drafts">;
  readonly signal: AbortSignal;
  readonly onState: (state: DraftObservationState) => void;
};

type Binding = {
  readonly key: string;
  readonly head: Dtos["HistoryHead"];
  readonly stop: AbortController;
};

const minimumRequestInterval = 75;
const maxRetryDelay = 12_000;

function delay(ms: number, signal: AbortSignal): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false);
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", cancel);
      resolve(true);
    }, ms);
    const cancel = () => {
      clearTimeout(timer);
      resolve(false);
    };
    signal.addEventListener("abort", cancel, { once: true });
  });
}

function requestFailure(error: unknown): Stop | "retry" {
  // The owner uses this application error for full observation slots, and the
  // gateway uses it while the original owner is rediscovered.
  if (error instanceof RemoteError && error.code === -32000) return "retry";
  if (!(error instanceof ClientError)) return "unavailable";
  switch (error.kind) {
    case "transport":
    case "timeout":
      return "retry";
    case "source":
    case "protocol":
      return "source";
    case "capability":
    case "request":
    case "limit":
      return "contract";
    case "http": {
      const status = error.status;
      if (status === 401) return "authentication";
      if (status === 403) return "authorization";
      if (status === 408 || status === 409 || status === 429 || (status !== undefined && status >= 500))
        return "retry";
      return "unavailable";
    }
    default:
      return "unavailable";
  }
}

function addressMatches(expected: FlowSource, value: FlowSource): boolean {
  const actual = flowSource(value);
  return expected.session_id === actual.session_id && expected.instance_id === actual.instance_id;
}

function bindHead(expected: FlowSource, page: Boundary): Dtos["HistoryHead"] {
  const branch = canonicalUuid(page.head.branch);
  if (!addressMatches(expected, page.flow) || !addressMatches(expected, page.source.flow) ||
    branch !== canonicalUuid(page.source.branch) || !Number.isSafeInteger(page.head.version) || page.head.version < 0)
    throw new ClientError("source", "Draft observation requires its verified conversation head");
  durableCursor(page.source.cursor, expected);
  return Object.freeze({ branch, version: page.head.version });
}

function edition(snapshot: Snapshot): Edition {
  return Object.freeze({
    host_generation: canonicalUuid(snapshot.host_generation),
    worker_incarnation: canonicalUuid(snapshot.worker_incarnation),
    revision: snapshot.revision,
  });
}

function sameOwner(previous: Edition, next: Edition): boolean {
  return previous.host_generation === next.host_generation &&
    previous.worker_incarnation === next.worker_incarnation;
}

/** Observe draft editions for one verified conversation, independently of its durable event stream. */
export function createDraftObserver(options: Options, wait: Wait = delay) {
  const { flow, signal, onState } = options;
  const source = flowSource(flow.address);
  let current: Binding | undefined;
  let closed = false;
  const valid = (binding: Binding) => current === binding && !closed && !binding.stop.signal.aborted;

  function close(): void {
    closed = true;
    const previous = current;
    current = undefined;
    previous?.stop.abort();
    signal.removeEventListener("abort", close);
  }

  if (signal.aborted) close();
  else signal.addEventListener("abort", close, { once: true });

  async function observe(binding: Binding): Promise<void> {
    const requestSignal = binding.stop.signal;
    let known: Edition | undefined;
    let lastCursor: Readonly<DurableCursor> | undefined;
    let retries = 0;
    let phase: Phase | undefined;
    let detached = false;

    const report = (state: Phase) => {
      if (valid(binding) && phase !== state) {
        phase = state;
        onState({ state });
      }
    };
    const retry = (state: "reconnecting" | "detached") => {
      report(state);
      const ceiling = Math.min(maxRetryDelay, 350 * 2 ** retries);
      retries = Math.min(retries + 1, 6);
      return wait(Math.round(ceiling * (0.85 + 0.15 * Math.random())), requestSignal);
    };
    const stop = (reason: Stop) => {
      if (valid(binding)) onState({ state: "stopped", reason });
    };

    while (valid(binding)) {
      const started = performance.now();
      let result: Dtos["ReadFlowDraftsResponse"];
      try {
        ({ result } = await flow.drafts(
          { head: binding.head, ...(known === undefined ? {} : { known }) },
          { signal: requestSignal },
        ));
      } catch (error) {
        if (!valid(binding)) return;
        const failure = requestFailure(error);
        if (failure !== "retry") { stop(failure); return; }
        if (!await retry("reconnecting")) return;
        continue;
      }
      if (!valid(binding)) return;

      if (result.state === "history_changed") {
        onState({ state: "stale_head" });
        return;
      }
      if (result.state === "unavailable") {
        detached = true;
        known = undefined;
        if (!await retry("detached")) return;
        continue;
      }
      if (result.state === "ready") {
        const { snapshot } = result;
        const next = edition(snapshot);
        const after = durableCursor(snapshot.after, source);
        const isSame = known !== undefined && sameOwner(known, next);
        const duplicate = isSame && known?.revision === next.revision;
        const olderReceipt = lastCursor !== undefined && (after.sequence < lastCursor.sequence ||
          (after.sequence === lastCursor.sequence && after.digest !== lastCursor.digest) ||
          (duplicate && after.sequence !== lastCursor.sequence));
        if (olderReceipt || (isSame && next.revision < (known?.revision ?? 0))) {
          stop("source");
          return;
        }
        const reset = detached || known !== undefined && !isSame;
        detached = false;
        known = next;
        lastCursor = after;
        if (!duplicate) {
          phase = "connected";
          onState({ state: "ready", snapshot, reset });
        } else {
          report("connected");
        }
      } else {
        report("connected");
      }
      retries = 0;
      const remaining = minimumRequestInterval - (performance.now() - started);
      if (remaining > 0 && !await wait(Math.ceil(remaining), requestSignal)) return;
    }
  }

  function bind(page: Boundary): void {
    if (closed) return;
    const head = bindHead(source, page);
    const key = `${head.branch}:${head.version}`;
    if (current?.key === key) return;
    const previous = current;
    const binding = { head, key, stop: new AbortController() };
    current = binding;
    previous?.stop.abort();
    if (previous !== undefined) onState({ state: "head_reset" });
    void observe(binding).catch(() => {
      if (valid(binding)) onState({ state: "stopped", reason: "source" });
    });
  }

  return Object.freeze({ bind, close });
}
