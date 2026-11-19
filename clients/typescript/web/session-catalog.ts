import type { AtmanClient, Dtos } from "../src/index.ts";

type SessionView = Dtos["SessionView"];
type FlowPage = Dtos["FlowListPage"];

export type SessionEdition = { view: SessionView; flows: FlowPage };

export function sameCatalog(left: Dtos["FlowCursor"], right: Dtos["FlowCursor"]): boolean {
  return left.journal_id === right.journal_id &&
    left.sequence === right.sequence &&
    left.digest === right.digest;
}

export function mergeFlowPages(current: FlowPage, page: FlowPage): FlowPage | undefined {
  if (!sameCatalog(current.catalog, page.catalog)) return undefined;
  const known = new Set(current.instances.map(({ flow }) => flow.instance_id));
  return {
    ...page,
    instances: [...current.instances, ...page.instances.filter(({ flow }) => !known.has(flow.instance_id))],
  };
}

export function initialFlow(view: SessionView, remembered?: string): string | undefined {
  return remembered ?? view.default_flow ?? undefined;
}

export class SessionCatalog {
  readonly #api: AtmanClient;
  readonly #signal: AbortSignal;
  readonly #cache = new Map<string, SessionEdition>();
  readonly #reads = new Map<string, { abort: AbortController; promise: Promise<SessionEdition> }>();

  constructor(api: AtmanClient, signal: AbortSignal) {
    this.#api = api;
    this.#signal = signal;
  }

  cached(id: string): SessionEdition | undefined {
    const entry = this.#cache.get(id);
    if (entry) this.store(entry);
    return entry;
  }

  store(entry: SessionEdition): void {
    this.#cache.delete(entry.view.session_id);
    if (entry.flows.instances.length > 250) return;
    this.#cache.set(entry.view.session_id, entry);
    if (this.#cache.size > 16) {
      const oldest = this.#cache.keys().next().value;
      if (oldest !== undefined) this.#cache.delete(oldest);
    }
  }

  refresh(id: string): Promise<SessionEdition> {
    const previous = this.#reads.get(id);
    if (previous) return previous.promise;
    if (this.#reads.size >= 4) {
      const oldest = this.#reads.keys().next().value;
      if (oldest !== undefined) {
        this.#reads.get(oldest)?.abort.abort();
        this.#reads.delete(oldest);
      }
    }
    const abort = new AbortController();
    const signal = AbortSignal.any([this.#signal, abort.signal]);
    const promise = this.#read(id, signal).finally(() => {
      if (this.#reads.get(id)?.abort === abort) this.#reads.delete(id);
    });
    this.#reads.set(id, { abort, promise });
    return promise;
  }

  async #read(id: string, signal: AbortSignal): Promise<SessionEdition> {
    const target = this.#api.session(id);
    const options = { signal };
    const { result: view } = await target.open(options);
    const { result: first } = await target.flows({ limit: 50 }, options);
    const previous = this.#cache.get(id);
    const preceding = previous?.flows;
    const fresh = new Map(first.instances.map((row) => [row.flow.instance_id, row]));
    const flows = preceding && preceding.instances.length > first.instances.length &&
        sameCatalog(preceding.catalog, first.catalog)
      ? { ...preceding, instances: preceding.instances.map((row) => fresh.get(row.flow.instance_id) ?? row) }
      : first;
    const entry = { view, flows };
    if (!signal.aborted) this.store(entry);
    return entry;
  }
}
