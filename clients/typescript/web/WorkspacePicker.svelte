<script lang="ts">
  import { onDestroy, tick } from "svelte";
  import { canonicalUuid } from "../src/index.ts";
  import type { AtmanClient, Dtos } from "../src/index.ts";
  import { waitForReadRetry } from "./durable-observer.ts";

  type Root = Dtos["WorkspaceRoot"];
  type FolderPage = Dtos["WorkspaceFolderPage"];
  type OpenRequest = Dtos["OpenWorkspaceRequest"];
  type Receipt = Dtos["WorkspaceOpenResponse"];
  type Ready = Extract<Dtos["WorkspaceOpenProgress"], { state: "conversation_ready" }>;
  type Connection = { api: AtmanClient; scope: string; signal: AbortSignal };
  type Pending = { scope: string; path: string; request: OpenRequest };
  type Branch = { expanded: boolean; names: readonly string[]; page?: FolderPage | undefined; busy?: boolean; error?: string | undefined };
  type Read = { root: string; parts: readonly string[]; controller: AbortController };
  type Navigation = () => boolean;
  type Props = {
    storage?: Storage | undefined;
    captureNavigation: () => Navigation;
    install: (source: AtmanClient, ready: Ready, signal: AbortSignal, current: Navigation) => Promise<boolean>;
    pinObserved: () => void;
    hasPending: (pending: boolean) => void;
    focusConversation: () => void;
  };
  let { storage, captureNavigation, install, pinObserved, hasPending, focusConversation }: Props = $props();

  const storageKey = "atman.web.workspace-open.v1";
  const fresh: Branch = { expanded: false, names: [] };
  const key = (root: string, parts: readonly string[]) => JSON.stringify([root, ...parts]);
  let dialog: HTMLDialogElement;
  let body = $state<HTMLDivElement>();
  let opener: HTMLElement | undefined;
  let connection: Connection | undefined;
  let visible: AbortController | undefined;
  let rootsRead: AbortController | undefined;
  const reads = new Map<string, Read>();
  const selections = new Map<string, readonly string[]>();
  let roots = $state.raw<readonly Root[]>([]);
  let root = $state.raw<Root | undefined>(undefined);
  let selected = $state.raw<readonly string[]>([]);
  let branches = $state.raw(new Map<string, Branch>());
  let rootBusy = $state(false);
  let rootError = $state("");
  let pending = $state.raw<Pending | undefined>(undefined);
  let receipt = $state.raw<Receipt | undefined>(undefined);
  let operation = $state<"checking" | "opening" | "installing" | undefined>();
  let viewOperation: symbol | undefined;
  let notice = $state("");
  let mayChooseAgain = $state(false);
  let storageFailed = $state(false);
  let installed = false;

  const branch = (rootId: string, parts: readonly string[]) => branches.get(key(rootId, parts)) ?? fresh;
  const setBranch = (rootId: string, parts: readonly string[], next: Branch) => {
    branches = new Map(branches).set(key(rootId, parts), next);
  };
  let selectedPage = $derived(root && branch(root.id, selected).page);
  let selectedPath = $derived(root && [root.label, ...selected].join(" / "));
  const browseError: Record<Dtos["WorkspaceBrowseFailure"], string> = {
    busy: "The host is busy. Try again.",
    directory_changed: "This folder changed. Refresh its listing.",
    directory_too_large: "This folder exceeds the host’s listing limit.",
    directory_unavailable: "This folder is unavailable.",
    invalid_policy: "Check workspace-roots.toml on the host.",
    unknown_root: "Access to this root has changed.",
    unsupported: "This host cannot browse workspace folders.",
  };
  const openError: Record<Dtos["WorkspaceOpenFailure"], string> = {
    selection_expired: "The folder selection expired.",
    directory_changed: "The selected folder or its access changed.",
    directory_unavailable: "The selected folder is unavailable.",
    invalid_configuration: "Check the workspace configuration on the host.",
    read_only_root: "Opening is disabled for this root.",
    project_unavailable: "The project could not be pinned.",
    session_unavailable: "The Session could not be created.",
    conversation_unavailable: "The Session exists. Its conversation could not start.",
    unsupported: "This host cannot open workspaces.",
  };
  const stageLabel: Record<Dtos["WorkspaceOpenProgress"]["state"], string> = {
    unseen: "Open unconfirmed",
    prepared: "Workspace accepted",
    project_pinned: "Project pinned",
    session_created: "Session created",
    conversation_ready: "Conversation ready",
  };

  function restore(scope: string): Pending | undefined {
    try {
      const saved = storage?.getItem(storageKey);
      if (!saved || saved.length > 30_000) return undefined;
      const value: Pending = JSON.parse(saved);
      if (value.scope !== scope || typeof value.path !== "string" || value.path.length > 8192) return undefined;
      const { operation_id, selection } = value.request;
      return { scope, path: value.path, request: { operation_id: canonicalUuid(operation_id), selection: canonicalUuid(selection) } };
    } catch {
      return undefined;
    }
  }

  function remember(value: Pending | undefined): void {
    pending = value;
    storageFailed = false;
    try {
      if (value) {
        if (storage) storage.setItem(storageKey, JSON.stringify(value));
        else storageFailed = true;
      } else if (connection && restore(connection.scope)) storage?.removeItem(storageKey);
    } catch {
      storageFailed = value !== undefined;
    }
    hasPending(value !== undefined);
  }

  export function bind(next: Connection): void {
    resetDialog();
    connection = next;
    receipt = undefined;
    notice = "";
    mayChooseAgain = false;
    operation = undefined;
    storageFailed = false;
    pending = restore(next.scope);
    hasPending(pending !== undefined);
  }

  export function unbind(clearPending: boolean): void {
    resetDialog();
    if (clearPending) remember(undefined);
    connection = undefined;
    pending = undefined;
    receipt = undefined;
    operation = undefined;
    hasPending(false);
  }

  function resetDialog(): void {
    visible?.abort();
    visible = undefined;
    if (viewOperation) {
      viewOperation = undefined;
      operation = undefined;
    }
    rootsRead?.abort();
    rootsRead = undefined;
    stopReads(() => true);
    roots = [];
    root = undefined;
    selected = [];
    branches = new Map();
    selections.clear();
    rootBusy = false;
    rootError = "";
    opener = undefined;
    if (dialog?.open) dialog.close();
  }

  onDestroy(resetDialog);

  export function show(trigger: HTMLElement): void {
    if (dialog.open || !connection || connection.signal.aborted) return;
    opener = trigger;
    installed = false;
    visible = new AbortController();
    dialog.showModal();
    if (pending) {
      if (!operation) void checkOpen();
    } else void loadRoots();
  }

  function close(): void {
    const trigger = opener;
    const wasInstalled = installed;
    resetDialog();
    void tick().then(() => {
      if (dialog.open) return;
      if (wasInstalled) focusConversation();
      else if (trigger?.isConnected && !trigger.closest("[inert]")) trigger.focus({ preventScroll: true });
    });
  }

  function stopReads(matches: (read: Read) => boolean): void {
    for (const [id, read] of reads) {
      if (!matches(read)) continue;
      reads.delete(id);
      read.controller.abort();
      setBranch(read.root, read.parts, { ...branch(read.root, read.parts), busy: false });
    }
  }

  async function loadRoots(): Promise<void> {
    const source = connection;
    const view = visible;
    if (!source || !view || rootBusy) return;
    const read = new AbortController();
    rootsRead = read;
    rootBusy = true;
    rootError = "";
    const signal = AbortSignal.any([source.signal, view.signal, read.signal]);
    try {
      const result = await source.api.listWorkspaceRoots({ signal });
      if (signal.aborted) return;
      if (result.state === "ready") {
        roots = result.directory.roots;
        const next = roots.find(({ id }) => id === root?.id) ?? roots[0];
        if (next) switchRoot(next);
        else root = undefined;
      } else rootError = browseError[result.reason];
    } catch {
      if (!signal.aborted) rootError = "Could not load workspace roots.";
    } finally {
      if (rootsRead === read) {
        rootsRead = undefined;
        rootBusy = false;
      }
    }
  }

  function changeRoot(event: Event & { currentTarget: HTMLSelectElement }): void {
    const next = roots.find(({ id }) => id === event.currentTarget.value);
    if (next) switchRoot(next);
  }

  function switchRoot(next: Root): void {
    stopReads((read) => read.root !== next.id);
    root = next;
    selected = selections.get(next.id) ?? [];
    setBranch(next.id, [], { ...branch(next.id, []), expanded: true });
    if (!branch(next.id, []).page) void loadFolders(next.id, []);
    if (selected.length > 0 && !branch(next.id, selected).page) void loadFolders(next.id, selected);
  }

  function selectFolder(rootId: string, parts: readonly string[]): void {
    selected = parts;
    selections.set(rootId, parts);
    const current = branch(rootId, parts);
    setBranch(rootId, parts, { ...current, expanded: true });
    if (!current.page || current.error) void loadFolders(rootId, parts);
  }

  function setExpanded(rootId: string, parts: readonly string[], expanded: boolean): void {
    const current = branch(rootId, parts);
    setBranch(rootId, parts, { ...current, expanded });
    if (expanded) {
      if (!current.page || current.error) void loadFolders(rootId, parts);
    } else {
      stopReads((read) => read.root === rootId && parts.every((part, index) => read.parts[index] === part));
    }
  }

  async function loadFolders(rootId: string, parts: readonly string[], more = false, focus = false): Promise<void> {
    const source = connection;
    const view = visible;
    const current = branch(rootId, parts);
    const id = key(rootId, parts);
    let after = more ? current.page?.after : undefined;
    if (!source || !view || root?.id !== rootId || reads.has(id) || (more && after == null)) return;
    const controller = new AbortController();
    const read = { root: rootId, parts, controller };
    reads.set(id, read);
    setBranch(rootId, parts, { ...current, busy: true, error: undefined });
    const signal = AbortSignal.any([source.signal, view.signal, controller.signal]);
    const currentRead = () => !signal.aborted && reads.get(id) === read && root?.id === rootId;
    const setError = (error: string) => setBranch(rootId, parts, { ...branch(rootId, parts), error });
    try {
      for (let attempts = 0; attempts < 4; attempts++) {
        const result = await source.api.listWorkspaceFolders({ root_id: rootId, components: [...parts], ...(after == null ? {} : { after }) }, { signal });
        if (!currentRead()) return;
        if (result.state === "ready") {
          const page = result.page;
          if (key(page.root_id, page.components) !== id) throw new Error("Workspace folder response target changed.");
          const names = after == null ? page.folders : [...new Set([...current.names, ...page.folders])];
          setBranch(rootId, parts, { ...branch(rootId, parts), names, page, error: undefined });
          if (focus) {
            const target = page.folders[0];
            await tick();
            if (currentRead()) focusFolder(rootId, target ? [...parts, target] : parts);
          }
          return;
        }
        if (result.reason === "directory_changed" && after != null) {
          after = undefined;
          continue;
        }
        if (result.reason === "busy" && attempts < 3 && await waitForReadRetry(90 * (attempts + 1), signal)) continue;
        if (currentRead()) setError(browseError[result.reason]);
        return;
      }
      if (currentRead()) setError("Could not refresh the folder.");
    } catch {
      if (currentRead()) setError("Could not load folders.");
    } finally {
      if (reads.get(id) === read) {
        reads.delete(id);
        setBranch(rootId, parts, { ...branch(rootId, parts), busy: false });
      }
    }
  }

  function focusFolder(rootId: string, parts: readonly string[]): void {
    if (!body) return;
    const folders = [...body.querySelectorAll<HTMLButtonElement>("button[data-folder]")];
    (folders.find((row) => row.dataset.folder === key(rootId, parts)) ?? folders[0])?.focus();
  }

  function folderKeys(event: KeyboardEvent, rootId: string, parts: readonly string[]): void {
    if (event.key === "ArrowRight") {
      setExpanded(rootId, parts, true);
    } else if (event.key === "ArrowLeft") {
      if (branch(rootId, parts).expanded) setExpanded(rootId, parts, false);
      else focusFolder(rootId, parts.slice(0, -1));
    } else if (body) {
      const folders = [...body.querySelectorAll<HTMLButtonElement>("button[data-folder]")];
      const index = folders.indexOf(event.currentTarget as HTMLButtonElement);
      const target = event.key === "ArrowDown" ? index + 1 : event.key === "ArrowUp" ? index - 1
        : event.key === "Home" ? 0 : event.key === "End" ? folders.length - 1 : undefined;
      if (target === undefined) return;
      folders[target]?.focus();
    } else return;
    event.preventDefault();
  }

  function openSelected(): void {
    const source = connection;
    if (!source || source.signal.aborted || !selectedPage || !selectedPath || pending || operation) return;
    receipt = undefined;
    remember({ scope: source.scope, path: selectedPath,
      request: { operation_id: crypto.randomUUID(), selection: selectedPage.selection } });
    void submitOpen();
  }

  function applyReceipt(next: Receipt): void {
    if (next.operation_id !== pending?.request.operation_id) throw new Error("Workspace operation identity changed.");
    receipt = next;
    notice = next.failure == null ? "" : openError[next.failure];
    mayChooseAgain = next.failure != null && next.progress.state === "unseen";
    if (next.progress.state === "project_pinned" || next.progress.state === "session_created" ||
        next.progress.state === "conversation_ready") pinObserved();
  }

  async function submitOpen(): Promise<void> {
    const source = connection;
    const original = pending;
    const view = visible;
    if (!source || !original || operation) return;
    const navigate = captureNavigation();
    const current = () => connection === source && pending === original && !source.signal.aborted;
    operation = "opening";
    notice = "";
    mayChooseAgain = false;
    let ready: Ready | undefined;
    try {
      const result = await source.api.openWorkspace(original.request, { signal: source.signal });
      if (!current()) return;
      applyReceipt(result);
      if (result.progress.state === "conversation_ready") ready = result.progress;
    } catch {
      if (!current()) return;
      notice = "Open unconfirmed. Check the original operation.";
      operation = undefined;
      if (visible) await checkOpen();
      return;
    }
    operation = undefined;
    if (ready && view && visible === view) await installReady(source, original, ready, view, navigate);
  }

  async function checkOpen(): Promise<void> {
    const source = connection;
    const original = pending;
    const view = visible;
    if (!source || !original || !view || operation) return;
    const token = Symbol();
    viewOperation = token;
    const signal = AbortSignal.any([source.signal, view.signal]);
    const current = () => viewOperation === token && !signal.aborted;
    operation = "checking";
    try {
      const result = await source.api.workspaceOpenStatus(original.request.operation_id, { signal });
      if (!current()) return;
      if (result.progress.state !== "unseen" || !receipt) applyReceipt(result);
    } catch {
      if (current()) notice = "Could not check the original operation.";
    } finally {
      if (viewOperation === token) {
        viewOperation = undefined;
        operation = undefined;
      }
    }
  }

  async function installReady(source: Connection, original: Pending, ready: Ready, view: AbortController, navigate: Navigation): Promise<void> {
    const signal = AbortSignal.any([source.signal, view.signal]);
    const current = () => !signal.aborted && connection === source && pending === original && visible === view && navigate();
    if (!current()) return;
    const token = Symbol();
    viewOperation = token;
    operation = "installing";
    notice = "";
    try {
      const attached = await install(source.api, ready, signal, current);
      if (attached && connection === source && pending === original && visible === view && !signal.aborted) {
        installed = true;
        remember(undefined);
        close();
      }
    } catch {
      if (current()) notice = "Could not open the created Session.";
    } finally {
      if (viewOperation === token) {
        viewOperation = undefined;
        operation = undefined;
      }
    }
  }

  function openReady(): void {
    if (!connection || !pending || !visible || operation || receipt?.progress.state !== "conversation_ready") return;
    void installReady(connection, pending, receipt.progress, visible, captureNavigation());
  }

  function chooseAgain(): void {
    if (!mayChooseAgain || operation) return;
    remember(undefined);
    receipt = undefined;
    notice = "";
    mayChooseAgain = false;
    if (root) {
      const current = branch(root.id, selected);
      setBranch(root.id, selected, { ...current, page: undefined });
      void loadFolders(root.id, selected);
    } else void loadRoots();
  }
</script>

{#snippet children(rootId: string, parent: readonly string[], depth: number)}
  {@const listing = branch(rootId, parent)}
  {#if listing.names.length}
    <ul class="folder-list">
      {#each listing.names as name (name)}
        {@const parts = [...parent, name]}
        {@const child = branch(rootId, parts)}
        {@const chosen = key(rootId, parts) === key(rootId, selected)}
        <li>
          <div class:chosen class="folder-row" style:--folder-indent={Math.min(depth, 8) + "rem"}>
            <button type="button" class="folder-disclosure" aria-expanded={child.expanded}
              aria-label={(child.expanded ? "Collapse " : "Expand ") + name}
              onclick={() => setExpanded(rootId, parts, !child.expanded)}>
              <span aria-hidden="true">{child.expanded ? "▾" : "▸"}</span>
            </button>
            <button type="button" class="folder-name" title={name} aria-pressed={chosen} data-folder={key(rootId, parts)}
              onkeydown={(event) => folderKeys(event, rootId, parts)} onclick={() => selectFolder(rootId, parts)}>
              {name}
            </button>
          </div>
          {#if child.expanded}{@render children(rootId, parts, depth + 1)}{/if}
        </li>
      {/each}
    </ul>
  {/if}
  {#if listing.busy}
    <p class="folder-detail" role="status" style:--folder-indent={Math.min(depth, 8) + "rem"}><span class="loading-mark" aria-hidden="true"></span> Loading folders…</p>
  {:else if listing.error}
    <div class="folder-detail folder-error" style:--folder-indent={Math.min(depth, 8) + "rem"}>
      <span role="status">{listing.error}</span><button type="button" onclick={() => void loadFolders(rootId, parent)}>Retry</button>
    </div>
  {:else if listing.page?.after != null}
    <div class="folder-detail" style:--folder-indent={Math.min(depth, 8) + "rem"}>
      <button type="button" onclick={(event) => void loadFolders(rootId, parent, true, event.detail === 0)}>More folders</button>
    </div>
  {:else if listing.page && !listing.names.length}
    <p class="folder-detail" style:--folder-indent={Math.min(depth, 8) + "rem"}>No subfolders</p>
  {/if}
{/snippet}

<dialog class="workspace-picker" bind:this={dialog} aria-labelledby="workspace-picker-title" oncancel={(event) => { event.preventDefault(); close(); }}>
  <div class="picker-layout">
    <header class="dialog-heading">
      <h2 id="workspace-picker-title">New Project</h2>
      <button type="button" class="icon" aria-label="Close" onclick={close}>×</button>
    </header>
    {#if pending}
      <div class="workspace-opening" aria-live="polite">
        <p class="workspace-path" title={pending.path}>{pending.path}</p>
        <p class="workspace-stage">
          {#if operation}<span class="loading-mark" aria-hidden="true"></span>{/if}
          {operation === "opening" ? "Opening workspace…" : operation === "checking" ? "Checking workspace…"
            : operation === "installing" ? "Opening Session…" : receipt ? stageLabel[receipt.progress.state] : "Open unconfirmed"}
        </p>
        {#if notice}<p class="workspace-error">{notice}</p>{/if}
        {#if storageFailed}<p class="dialog-copy">This tab cannot restore the operation after refresh.</p>{/if}
      </div>
      <footer class="workspace-actions">
        {#if mayChooseAgain}
          <button type="button" onclick={chooseAgain} disabled={operation !== undefined}>Choose folder</button>
        {:else if receipt?.progress.state === "conversation_ready"}
          <button type="button" class="primary" onclick={openReady} disabled={operation !== undefined}>Open Session</button>
        {:else}
          <button type="button" onclick={() => void checkOpen()} disabled={operation !== undefined}>Check status</button>
          <button type="button" class="primary" onclick={() => void submitOpen()} disabled={operation !== undefined}>Retry open</button>
        {/if}
      </footer>
    {:else}
      {#if roots.length > 1 && root}
        <select aria-label="Workspace root" value={root.id} onchange={changeRoot}>
          {#each roots as entry (entry.id)}<option value={entry.id}>{entry.label}</option>{/each}
        </select>
      {/if}
      {#if root}
        {@const currentRoot = root}
        <nav class="folder-crumbs" aria-label="Folder path">
          <button type="button" onclick={() => selectFolder(currentRoot.id, [])}>{currentRoot.label}</button>
          {#each selected as part, index (index)}
            <span aria-hidden="true">/</span>
            <button type="button" onclick={() => selectFolder(currentRoot.id, selected.slice(0, index + 1))}>{part}</button>
          {/each}
        </nav>
        <div class="folder-tree" role="region" aria-label="Workspace folders" bind:this={body}>
          {@render children(currentRoot.id, [], 0)}
        </div>
      {:else}
        <div class="workspace-opening" role="status">
          {#if rootBusy}
            <p><span class="loading-mark" aria-hidden="true"></span> Loading workspace roots…</p>
          {:else if rootError}
            <p class="workspace-error">{rootError}</p>
            <button type="button" onclick={() => void loadRoots()}>Retry</button>
          {:else}
            <p>No workspace roots</p>
            <p class="dialog-copy">Add a root in workspace-roots.toml on this host.</p>
          {/if}
        </div>
      {/if}
      <footer class="workspace-actions">
        {#if selectedPath}<p class="workspace-path" title={selectedPath}>{selectedPath}</p>{/if}
        <button type="button" class="primary" disabled={!selectedPage} onclick={openSelected}>Open workspace</button>
      </footer>
    {/if}
  </div>
</dialog>

<style>
  .workspace-picker { width: min(36rem, calc(100vw - 1.5rem)); max-block-size: calc(100dvh - 1.5rem); padding: clamp(0.85rem, 4vw, 1.35rem); }
  .picker-layout { display: grid; gap: 0.65rem; }
  select {
    color: inherit;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 3px;
    font: inherit;
    padding: 0.4rem 0.55rem;
    max-inline-size: 100%;
    justify-self: start;
  }
  select:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .folder-crumbs {
    display: flex;
    align-items: center;
    gap: 0.15rem;
    color: var(--muted);
    overflow-x: auto;
    min-inline-size: 0;
  }
  .folder-crumbs button { flex: none; border-color: transparent; padding: 0.25rem 0.4rem; max-inline-size: 13rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .folder-crumbs button:last-child { color: var(--fg); }
  .folder-tree {
    block-size: clamp(8rem, 38dvh, 20rem);
    overflow: auto;
    border-block: 1px solid var(--line);
    padding: 0.35rem;
  }
  .folder-list { list-style: none; padding: 0; margin: 0; }
  .folder-row {
    display: grid;
    grid-template-columns: 2rem minmax(0, 1fr);
    padding-inline-start: var(--folder-indent);
    border-inline-start: 2px solid transparent;
  }
  .folder-row.chosen { background: var(--hover); border-inline-start-color: var(--accent); }
  .folder-row button { padding-block: 0.4rem; border-color: transparent; }
  .folder-disclosure { color: var(--muted); padding-inline: 0.4rem; }
  .folder-name { text-align: start; padding-inline: 0.45rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .folder-detail {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem;
    color: var(--muted);
    min-block-size: 2rem;
    padding-inline-start: min(calc(2.5rem + var(--folder-indent)), 40%);
    font-size: 0.88rem;
  }
  .folder-detail button { padding: 0.28rem 0.55rem; }
  .folder-error, .workspace-error { color: var(--danger); }
  .workspace-opening { display: grid; gap: 0.55rem; padding: 0.65rem 0; }
  .workspace-opening button { justify-self: start; }
  .workspace-stage { color: var(--muted); }
  .workspace-actions { display: flex; justify-content: flex-end; align-items: center; flex-wrap: wrap; min-inline-size: 0; gap: 0.45rem; }
  .workspace-actions .workspace-path { margin-inline-end: auto; }
  .workspace-path { min-inline-size: 0; color: var(--muted); overflow-wrap: anywhere; font-size: 0.9rem; }
</style>
