<script lang="ts">
  import { onDestroy, tick } from "svelte";
  import { ClientError, RemoteError, cursorId, decodeDto } from "../src/index.ts";
  import type { AtmanClient, Dtos, FlowClient } from "../src/index.ts";
  import { waitForReadRetry } from "./durable-observer.ts";

  type Intent = Dtos["ForkSourceReviewRequest"];
  type Review = Dtos["ForkSourceReviewResponse"];
  type Request = Dtos["CreateForkRequest"];
  type Progress = Dtos["ForkProgress"];
  type Ready = Extract<Progress, { state: "ready" }>;
  type Connection = { api: AtmanClient; scope: string; signal: AbortSignal };
  type Source = { flow: FlowClient; intent: Intent; label: string; current: () => boolean };
  type Props = {
    storage?: Storage | undefined;
    hasPending: (pending: boolean) => void;
    install: (api: AtmanClient, ready: Ready, signal: AbortSignal) => Promise<boolean>;
    focusConversation: () => void;
  };
  let { storage, hasPending, install, focusConversation }: Props = $props();

  const storageKey = "atman.web.fork.v1";
  const capacity = 16_384;
  const encoder = new TextEncoder();
  const number = new Intl.NumberFormat();
  const errorText = (error: unknown) => error instanceof Error ? error.message : "Fork request failed.";

  let dialog: HTMLDialogElement;
  let connection: Connection | undefined;
  let observation: AbortController | undefined;
  let trigger: HTMLButtonElement | undefined;
  let source = $state.raw<Source | undefined>(undefined);
  let pending = $state.raw<Request | undefined>(undefined);
  let review = $state.raw<Review | undefined>(undefined);
  let progress = $state.raw<Progress | undefined>(undefined);
  let action = $state<"reviewing" | "checking" | "creating" | "opening" | undefined>();
  let title = $state("");
  let location = $state<Dtos["ForkLocation"]>("source_project");
  let notice = $state("");
  let memoryOnly = $state(false);
  let canReviewAgain = $state(false);
  let validTitle = $derived(title.trim().length > 0 && encoder.encode(title.trim()).length <= 256 && !/\p{Cc}/u.test(title));
  let projectDirectory = $derived(review && review.flow_project === review.session_project && review.working_directory &&
    inside(review.working_directory, review.session_project) ? review.working_directory : review?.session_project);
  let directoryProject = $derived(review && inside(review.working_directory, review.flow_project) ? review.flow_project : undefined);
  let chooseLocation = $derived(review?.session_project && review.working_directory &&
    (projectDirectory !== review.working_directory || directoryProject !== review.session_project));

  function inside(directory: string | null | undefined, root: string | null | undefined): boolean {
    if (!root || !directory) return false;
    const normalize = (path: string) => path.replaceAll("\\", "/").replace(/\/$/, "");
    const parent = normalize(root);
    const child = normalize(directory);
    return child === parent || child.startsWith(parent + "/");
  }

  function read(scope: string): Request | undefined {
    try {
      const raw = storage?.getItem(storageKey);
      if (!raw || raw.length > capacity) return undefined;
      const entry = JSON.parse(raw);
      if (entry.scope === scope) return decodeDto("CreateForkRequest", entry.request);
    } catch { /* The host remains the authority for valid archived requests. */ }
  }

  function save(request: Request | undefined): void {
    pending = request;
    memoryOnly = false;
    try {
      if (request && connection) {
        const encoded = JSON.stringify({ scope: connection.scope, request });
        if (!storage || encoded.length > capacity) memoryOnly = true;
        else storage.setItem(storageKey, encoded);
      } else if (connection && read(connection.scope)) storage?.removeItem(storageKey);
    } catch { memoryOnly = request !== undefined; }
    hasPending(request !== undefined);
  }

  export function bind(next: Connection): void {
    unbind(false);
    connection = next;
    pending = read(next.scope);
    hasPending(pending !== undefined);
    if (pending) void reconcile();
  }

  export function unbind(clear: boolean): void {
    hide(false);
    if (clear) save(undefined);
    connection = undefined;
    pending = undefined;
    progress = undefined;
    action = undefined;
    notice = "";
    memoryOnly = false;
    canReviewAgain = false;
    hasPending(false);
  }

  export function show(origin: HTMLButtonElement, selected?: Source): void {
    if (!connection || connection.signal.aborted || dialog.open || (!pending && !selected?.current())) return;
    trigger = origin;
    source = selected;
    observation = new AbortController();
    dialog.showModal();
    if (!pending) void loadReview();
    else if (!action && progress?.state !== "ready") void reconcile();
  }

  export function focusChanged(): void {
    if (source && !source.current()) hide(false);
  }

  function hide(restore = true): void {
    const origin = trigger;
    trigger = undefined;
    source = undefined;
    observation?.abort();
    observation = undefined;
    review = undefined;
    if (!pending) notice = "";
    if (action === "reviewing" || action === "opening") action = undefined;
    if (dialog?.open) dialog.close();
    if (restore && origin?.isConnected && !origin.closest("[inert]")) origin.focus({ preventScroll: true });
  }

  onDestroy(() => hide(false));

  async function loadReview(): Promise<void> {
    const target = source;
    const view = observation;
    const connected = connection;
    if (!target?.current() || !view || !connected || pending || action) return;
    const signal = AbortSignal.any([connected.signal, view.signal]);
    review = undefined;
    notice = "";
    action = "reviewing";
    try {
      const { result } = await target.flow.forkSourceReview(target.intent, { signal });
      if (!signal.aborted && target.current()) {
        title = "Fork of " + target.label;
        while (encoder.encode(title).length > 256) title = Array.from(title).slice(0, -1).join("");
        location = result.session_project ? "source_project" : "source_working_directory";
        review = result;
      }
    } catch (error) {
      if (!signal.aborted) notice = errorText(error);
    } finally {
      if (!signal.aborted && observation === view) action = undefined;
    }
  }

  function confirm(event: SubmitEvent): void {
    event.preventDefault();
    if (!source?.current()) {
      notice = "Source branch changed. Close and review the current branch.";
      return;
    }
    if (!review || !validTitle || pending || action ||
      (location === "source_project" ? !review.session_project : !review.working_directory)) return;
    save({
      operation_id: crypto.randomUUID(),
      review: source.intent,
      source: review.source,
      catalog: review.catalog,
      asset_copy_bytes: review.asset_copy_bytes,
      location,
      title: title.trim(),
    });
    review = undefined;
    progress = undefined;
    void reconcile(true);
  }

  function accept(next: Progress): void {
    if (progress && progress.state !== "unseen" && (next.state === "unseen" ||
      next.destination.session_id !== progress.destination.session_id ||
      next.destination.instance_id !== progress.destination.instance_id)) {
      throw new Error("The host returned a different Fork destination.");
    }
    if (progress?.state === "ready" && (next.state !== "ready" ||
      cursorId(next.activation) !== cursorId(progress.activation) ||
      next.publication_digest !== progress.publication_digest)) {
      throw new Error("The host returned a different Fork publication.");
    }
    progress = next;
  }

  async function reconcile(create = false): Promise<void> {
    const request = pending;
    const connected = connection;
    if (!request || !connected || connected.signal.aborted || action) return;
    const { api, signal } = connected;
    const current = () => !signal.aborted && connected === connection && request === pending;
    notice = "";
    canReviewAgain = false;
    action = create ? "creating" : "checking";
    const flow = api.flow(request.review.observed.flow);
    let reviewRejected = false;
    try {
      if (create) {
        try {
          const { result } = await flow.createFork(request, { signal });
          if (!current()) return;
          accept(result.progress);
        } catch (error) {
          if (!current()) return;
          notice = errorText(error);
          reviewRejected = error instanceof RemoteError &&
            (error.message === "Fork source or project changed after review" || error.code === -32002);
          if (error instanceof ClientError && error.outcome === "not_sent" && progress === undefined) {
            save(undefined);
            action = undefined;
            return;
          }
        }
        if (progress?.state === "ready" || progress?.state === "retry_required") return;
        action = "checking";
      }
      for (let attempt = 0; attempt < 5; attempt++) {
        const { result } = await flow.forkStatus(request.operation_id, { signal });
        if (!current()) return;
        accept(result.progress);
        if (result.progress.state === "ready" || result.progress.state === "preparing") notice = "";
        canReviewAgain = reviewRejected && result.progress.state === "unseen";
        if (result.progress.state !== "preparing" || !dialog.open || attempt === 4 ||
          !await waitForReadRetry(250 * (attempt + 1), signal)) return;
      }
    } catch (error) {
      if (current()) notice = errorText(error);
    } finally {
      if (current()) action = undefined;
    }
  }

  function reviewAgain(): void {
    if (!canReviewAgain || progress?.state !== "unseen" || action) return;
    save(undefined);
    progress = undefined;
    hide();
  }

  async function open(): Promise<void> {
    const connected = connection;
    const view = observation;
    if (!connected || !view || progress?.state !== "ready" || action) return;
    const signal = AbortSignal.any([connected.signal, view.signal]);
    action = "opening";
    notice = "";
    try {
      if (await install(connected.api, progress, signal)) {
        save(undefined);
        progress = undefined;
        hide(false);
        await tick();
        if (!dialog.open && connected === connection) focusConversation();
      }
    } catch (error) {
      if (!signal.aborted) notice = errorText(error);
    } finally {
      if (!signal.aborted && view === observation) action = undefined;
    }
  }

  let boundary = $derived(review?.selected?.kind === "before_user_turn" ? "Before selected turn"
    : source?.intent.cutoff?.kind === "after_completed_turn" ? "After selected completed turn"
    : review?.selected ? "Latest completed turn" : "Before the first turn");
</script>

<dialog bind:this={dialog} class="fork-dialog" aria-labelledby="fork-title"
  oncancel={(event) => { event.preventDefault(); hide(); }}>
  <section>
    <header>
      <h2 id="fork-title">Fork Session</h2>
      <button type="button" class="icon" aria-label="Hide Fork" title="Hide Fork" onclick={() => hide()}>×</button>
    </header>
    {#if pending}
      <p class="fork-name">{pending.title}</p>
      <p role="status">{action === "creating" ? "Creating Fork…"
        : action === "opening" ? "Opening Fork…" : action === "checking" ? "Checking the original Fork…"
        : progress?.state === "ready" ? "Fork is ready." : progress?.state === "preparing" ? "The host is preparing the Fork."
        : progress?.state === "retry_required" ? "The Fork can be resumed." : "Fork creation is unconfirmed."}</p>
      {#if memoryOnly}<p role="status">Keep this tab open to retain the original request.</p>{/if}
      <div class="actions">
        {#if progress?.state === "ready"}
          <button type="button" class="primary" disabled={action !== undefined} onclick={open}>Open Fork</button>
        {:else}
          <button type="button" disabled={action !== undefined} onclick={() => reconcile()}>Check status</button>
          {#if !canReviewAgain && (progress?.state === "retry_required" || progress?.state === "unseen")}
            <button type="button" class="primary" disabled={action !== undefined} onclick={() => reconcile(true)}>Retry original Fork</button>
          {/if}
          {#if canReviewAgain}<button type="button" onclick={reviewAgain}>Close and review again</button>{/if}
        {/if}
      </div>
    {:else if review}
      <form onsubmit={confirm}>
        <label>Title <input aria-label="Fork title" bind:value={title} maxlength="256" required /></label>
        <dl>
          <dt>Source</dt><dd>{source?.label}</dd>
          <dt>Boundary</dt><dd>{boundary}</dd>
          {#if review.excluded_turns > 0}<dt>Turns excluded</dt><dd>{number.format(review.excluded_turns)}</dd>{/if}
          <dt>History records</dt><dd>{number.format(review.history_records)}</dd>
          <dt>Snapshot</dt><dd>{number.format(review.snapshot_bytes)} B</dd>
          <dt>Asset copy limit</dt><dd>{number.format(review.asset_copy_bytes)} B</dd>
        </dl>
        {#if chooseLocation}
          <label>Destination
            <select aria-label="Fork destination" bind:value={location}>
              <option value="source_project">Current Session project</option>
              <option value="source_working_directory">Source Flow working directory</option>
            </select>
          </label>
        {/if}
        <dl>
          <dt>Project</dt><dd>{(location === "source_project" ? review.session_project : directoryProject) || "Resolved by host"}</dd>
          <dt>Working directory</dt><dd>{(location === "source_project" ? projectDirectory : review.working_directory) || "Unavailable"}</dd>
        </dl>
        <p>The new Session waits for your first message. The source continues independently.</p>
        <button type="submit" class="primary" disabled={!validTitle || (!review.session_project && !review.working_directory)}>Create Fork</button>
      </form>
    {:else if action === "reviewing"}
      <p role="status">Reading the committed conversation…</p>
    {:else if source?.current()}
      <button type="button" onclick={loadReview}>Check boundary again</button>
    {/if}
    {#if notice}<p role="alert">{notice}</p>{/if}
  </section>
</dialog>

<style>
  .fork-dialog { max-block-size: min(80dvh, 38rem); overflow: auto; padding: clamp(1rem, 5vw, 1.5rem); }
  section, form { display: grid; gap: .85rem; min-inline-size: 0; }
  header, .actions { display: flex; align-items: center; gap: .6rem; }
  header { justify-content: space-between; }
  .actions { flex-wrap: wrap; }
  h2, p, dl, dd { margin: 0; }
  h2, .fork-name { font-size: 1rem; }
  dl { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: .35rem .8rem; }
  label { display: grid; gap: .3rem; }
  input, select { min-inline-size: 0; width: 100%; }
  dt, p { color: var(--muted); }
  dd, p { overflow-wrap: anywhere; }
  .fork-name { color: var(--fg); }
  [role="alert"] { color: var(--danger); }
  button { justify-self: start; padding: .25rem .65rem; }
</style>
