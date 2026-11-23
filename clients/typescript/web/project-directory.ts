import type { AtmanClient, Dtos } from "../src/index.ts";

export type ProjectSort = Dtos["ProjectSessionSort"];
type Rows = Dtos["ProjectDirectoryRows"];
type Result = Dtos["ReadProjectDirectoryResponse"];
type Query = Dtos["ReadProjectDirectoryRequest"];
type Group = Dtos["ProjectDirectoryGroup"];
type Session = Dtos["ProjectSessionEntry"];
type Published<T> = { rows: readonly T[]; page: Dtos["ProjectDirectoryPage"] };
type Feed<T> = { value?: Published<T> | undefined; read?: AbortController | undefined; error?: string | undefined };

const projectLimit = 50;
const sessionLimit = 20;

export class ProjectDirectory {
  #source: { api: AtmanClient; signal: AbortSignal } | undefined;
  #query = "";
  #groups: Feed<Group> = {};
  readonly #sessions = new Map<string, Feed<Session>>();
  readonly #sorts = new Map<string, ProjectSort>();
  #typing: ReturnType<typeof setTimeout> | undefined;

  constructor(readonly changed: () => void) {}

  bind(api: AtmanClient, signal: AbortSignal): void {
    this.close();
    this.#source = { api, signal };
  }

  close(): void {
    this.#source = undefined;
    this.#clearReads();
    this.#sorts.clear();
    this.#query = "";
  }

  #clearReads(): void {
    clearTimeout(this.#typing);
    this.#typing = undefined;
    this.#groups.read?.abort();
    this.#groups = {};
    for (const feed of this.#sessions.values()) feed.read?.abort();
    this.#sessions.clear();
  }

  get groups(): Readonly<Feed<Group>> { return this.#groups; }
  get loadingQuery(): boolean { return this.#typing !== undefined || this.#groups.read !== undefined; }

  sessions(root: string): Readonly<Feed<Session>> | undefined { return this.#sessions.get(root); }
  sort(root: string): ProjectSort { return this.#sorts.get(root) ?? "newest_first"; }

  search(text: string): void {
    const query = text.trim();
    if (query === this.#query) return;
    this.#clearReads();
    this.#query = query;
    this.#typing = setTimeout(() => {
      this.#typing = undefined;
      void this.loadGroups();
    }, 120);
    this.changed();
  }

  loadGroups(more = false, refresh = false): Promise<void> {
    clearTimeout(this.#typing);
    this.#typing = undefined;
    return this.#load(this.#groups, { target: { kind: "projects" }, limit: projectLimit },
      (rows) => rows.kind === "projects" ? rows.projects : [], more, refresh);
  }

  async refresh(): Promise<void> {
    const source = this.#source;
    const groups = this.#groups;
    await this.loadGroups(false, true);
    if (this.#source !== source || this.#groups !== groups) return;
    for (const root of [...this.#sessions.keys()]) void this.loadSessions(root);
  }

  loadSessions(root: string, more = false): Promise<void> {
    let feed = this.#sessions.get(root);
    if (!feed) {
      if (this.#sessions.size === 24) {
        const oldest = this.#sessions.keys().next().value!;
        this.#sessions.get(oldest)?.read?.abort();
        this.#sessions.delete(oldest);
      }
      feed = {};
    }
    this.#sessions.delete(root);
    this.#sessions.set(root, feed);
    return this.#load(feed, { target: { kind: "sessions", project_root: root || null }, sort: this.sort(root), limit: sessionLimit },
      (rows) => rows.kind === "sessions" ? rows.sessions : [], more, false);
  }

  setSort(root: string, sort: ProjectSort): void {
    if (sort === this.sort(root)) return;
    this.#sorts.set(root, sort);
    this.#sessions.get(root)?.read?.abort();
    this.#sessions.delete(root);
    void this.loadSessions(root);
  }

  catalogFor(project: Dtos["ProjectListEntry"]): string | undefined {
    const observed = this.#groups.value;
    return observed?.rows.some((group) => group.record === project) ? observed.page.catalog_revision : undefined;
  }

  applyPin(old: Dtos["ProjectListEntry"], receipt: Dtos["SetProjectPinnedResponse"]): void {
    const observed = this.#groups.value;
    if (!observed || !this.catalogFor(old)) return;
    this.#groups.value = { page: { ...observed.page, catalog_revision: receipt.revision },
      rows: observed.rows.map((row) => row.record === old ? { ...row, record: receipt.project } : row) };
    this.changed();
  }

  async #load<T>(feed: Feed<T>, selection: Query, select: (rows: Rows) => readonly T[], more: boolean, refresh: boolean): Promise<void> {
    const source = this.#source;
    const original = feed.value;
    const after = more ? original?.page.after : undefined;
    if (!source || source.signal.aborted || (more && (feed.read || after == null))) return;
    feed.read?.abort();
    const read = new AbortController();
    const signal = AbortSignal.any([source.signal, read.signal]);
    const current = () => this.#source === source && feed.read === read && !signal.aborted;
    const query = { ...selection, ...(this.#query ? { search: this.#query } : {}) };
    feed.read = read;
    feed.error = undefined;
    this.changed();
    try {
      const fetch = (cursor?: Dtos["ProjectListCursor"], fresh = false): Promise<Result> => source.api.readProjectDirectory({
        ...query, ...(cursor === undefined ? (fresh ? { refresh: true } : {}) : { after: cursor }),
      }, { signal });
      let result = await fetch(after ?? undefined, refresh);
      let append = after != null;
      if (!current()) return;
      if (result.state === "changed") {
        append = false;
        result = await fetch();
      }
      if (!current()) return;
      if (result.state !== "ready") throw new Error(result.state === "unknown_project" ? "Project is no longer in this directory." : "Session directory changed.");
      let page = result.page;
      const rows = [...(append ? original?.rows ?? [] : []), ...select(page.rows)];
      const retained = (original?.rows.length ?? 0) + (after == null ? 0 : selection.limit ?? 0);
      while (!append && rows.length < retained && page.after != null) {
        const next = await fetch(page.after);
        if (!current()) return;
        if (next.state !== "ready") break;
        page = next.page;
        rows.push(...select(page.rows));
      }
      feed.value = { page, rows };
    } catch (error) {
      if (current()) feed.error = error instanceof Error ? error.message : "Could not load the directory.";
    } finally {
      if (current()) {
        feed.read = undefined;
        this.changed();
      }
    }
  }
}
