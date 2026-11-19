import { ClientError, canonicalUuid, durableCursor, flowSource } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient, FlowSource } from "../src/index.ts";

type Subscription = Awaited<ReturnType<FlowClient["observeEvents"]>>;
type Snapshot = Dtos["ConversationPage"];
type Continuation = "retry" | "reset";
type Stopped = "authentication" | "authorization" | "contract" | "source" | "unavailable";

export type DurableObservationState =
  | { readonly state: "connected" | "reconnecting" | "checking_source" }
  | { readonly state: "stopped"; readonly reason: Stopped };

interface DurableObservation {
  readonly source: FlowSource;
  readonly after: Readonly<DurableCursor>;
  readonly signal: AbortSignal;
  readonly open: (after: Readonly<DurableCursor>, signal: AbortSignal) => Promise<Subscription>;
  /** Return the fresh, checked page installed by the original view. */
  readonly refresh: (signal: AbortSignal) => Promise<Snapshot | undefined>;
  readonly onBatch: (last: Readonly<DurableCursor>) => void;
  readonly onState: (state: DurableObservationState) => void;
}

type Wait = (delay: number, signal: AbortSignal) => Promise<boolean>;

function waitToRetry(delay: number, signal: AbortSignal): Promise<boolean> {
  if (signal.aborted) return Promise.resolve(false);
  return new Promise((resolve) => {
    const slept = () => {
      signal.removeEventListener("abort", stop);
      resolve(true);
    };
    const timer = setTimeout(slept, delay);
    const stop = () => {
      clearTimeout(timer);
      resolve(false);
    };
    signal.addEventListener("abort", stop, { once: true });
  });
}

function recovery(error: unknown): Continuation | Stopped {
  if (!(error instanceof ClientError)) return "unavailable";
  switch (error.kind) {
    case "transport":
    case "timeout":
      return "retry";
    case "source":
    case "protocol":
      return "reset";
    case "capability":
    case "limit":
    case "request":
      return "contract";
    case "http": {
      const status = error.status;
      if (status === 401) return "authentication";
      if (status === 403) return "authorization";
      if (status === 400 || status === 410) return "reset";
      // A racing host transfer can return 409 before the gateway rediscovers the owner.
      if (status === 408 || status === 409 || status === 429 || (status !== undefined && status >= 500))
        return "retry";
      return "unavailable";
    }
    default:
      return "unavailable";
  }
}

function checkSnapshot(page: Snapshot, source: FlowSource): Readonly<DurableCursor> {
  const matches = (address: FlowSource) =>
    canonicalUuid(address.session_id) === source.session_id &&
    canonicalUuid(address.instance_id) === source.instance_id;
  if (!matches(page.flow) || !matches(page.source.flow) ||
    canonicalUuid(page.head.branch) !== canonicalUuid(page.source.branch))
    throw new ClientError("source", "Conversation snapshot belongs to another observation");
  return durableCursor(page.source.cursor, source);
}

/** This task owns one view's read lease. A reconnect or reset never invokes a command. */
export async function observeDurable(
  options: DurableObservation,
  wait: Wait = waitToRetry,
): Promise<void> {
  const { signal, onState } = options;
  if (signal.aborted) return;
  const source = flowSource(options.source);
  let after = durableCursor(options.after, source);
  let retries = 0;
  let checkingSource = false;
  let resetSinceDelivery = false;
  let reported: DurableObservationState["state"] | undefined;
  const emit = (state: DurableObservationState) => {
    if (!signal.aborted && state.state !== reported) {
      reported = state.state;
      onState(state);
    }
  };

  while (!signal.aborted) {
    let next: Continuation | Stopped;
    if (checkingSource) {
      try {
        const page = await options.refresh(signal);
        if (signal.aborted) return;
        if (page === undefined) {
          emit({ state: "stopped", reason: "source" });
          return;
        }
        after = checkSnapshot(page, source);
        checkingSource = false;
        continue;
      } catch (error) {
        next = recovery(error);
        if (next === "reset") next = "source";
      }
    } else {
      let subscription: Subscription | undefined;
      let connectedAt: number | undefined;
      try {
        subscription = await options.open(after, signal);
        if (signal.aborted) return;
        connectedAt = performance.now();
        emit({ state: "connected" });
        for await (const _batch of subscription) {
          if (signal.aborted) return;
          const delivered = subscription.last;
          if (delivered === undefined) throw new ClientError("source", "Delivered event has no cursor");
          after = durableCursor(delivered, source);
          options.onBatch(after);
          retries = 0;
          resetSinceDelivery = false;
        }
        next = "retry";
      } catch (error) {
        next = recovery(error);
      } finally {
        await subscription?.close().catch(() => {});
      }
      if (connectedAt !== undefined && performance.now() - connectedAt >= 12_000) retries = 0;
    }
    if (signal.aborted) return;

    if (next === "reset") {
      if (resetSinceDelivery) {
        emit({ state: "stopped", reason: "source" });
        return;
      }
      resetSinceDelivery = true;
      checkingSource = true;
    } else if (next !== "retry") {
      emit({ state: "stopped", reason: next });
      return;
    }

    emit({ state: checkingSource ? "checking_source" : "reconnecting" });
    const ceiling = Math.min(12_000, 300 * 2 ** retries);
    retries = Math.min(retries + 1, 6);
    if (!await wait(Math.round(ceiling * (0.8 + 0.2 * Math.random())), signal)) return;
  }
}
