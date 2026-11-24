<script lang="ts">
  import type { Dtos } from "../src/index.ts";
  import type { HistoryAction, HistoryView } from "./history-ui.ts";

  let { dispatch, hide }: {
    dispatch: (action: HistoryAction) => void;
    hide: () => void;
  } = $props();

  let snapshot = $state.raw<HistoryView | undefined>();
  export function update(next: HistoryView): void { snapshot = next; }

  const count = (value: number, noun: string) => value + " " + noun + (value === 1 ? "" : "s");
  const short = (id: string) => id.slice(0, 8);
  const recoverable = (review: Dtos["HistoryReviewResponse"]) => review.recoverable_inputs?.length ?? 0;
  let title = $derived(snapshot?.kind === "branches" ? "Branches" : snapshot?.mode === "revert" ? "Revert conversation" : "Restore conversation");
</script>

{#if snapshot}
{@const view = snapshot}
<section class="history-panel" aria-labelledby="history-title">
  <header>
    <h2 id="history-title">{title}</h2>
    <button type="button" aria-label="Hide history panel" title="Hide history panel" onclick={hide}>×</button>
  </header>
  {#if view.kind === "branches"}
    <ul aria-label="Saved branches">
      {#each view.branches as branch (branch.id)}
        <li>
          <span title={branch.id}>{short(branch.id)}</span>
          {#if branch.id === view.head.branch}<span class="muted" aria-current="true">Current</span>
          {:else}<button type="button" disabled={view.loading} onclick={() => dispatch({ kind: "review_restore", head: view.head, branch: branch.id })}>Review restore<span class="sr-only"> {short(branch.id)}</span></button>{/if}
        </li>
      {/each}
    </ul>
    {#if view.drafts.length}
      <ul aria-label="Saved draft exchanges">
        {#each view.drafts as draft, index (draft.operation + draft.input)}
          <li>
            <span>Recovered input{view.drafts.length > 1 ? " " + (index + 1) : ""}</span>
            <button type="button" disabled={draft.action === "blocked"}
              onclick={() => dispatch({ kind: "swap_saved", head: view.head, identity: draft })}>{draft.action === "return" ? "Return to draft" : "Swap drafts"}</button>
          </li>
        {/each}
      </ul>
    {/if}
    {#if !view.loading && !view.error && view.branches.length === 0}<p>No saved branches.</p>{/if}
    {#if view.loading}<p role="status">Loading branches…</p>{/if}
    {#if view.error}
      <p role="alert">{view.error}</p>
      <button type="button" onclick={() => dispatch({ kind: "reload_branches", head: view.head })}>Try again</button>
    {:else if view.next != null}
      {@const offset = view.next}
      <button type="button" disabled={view.loading} onclick={() => dispatch({ kind: "more_branches", head: view.head, offset })}>More branches</button>
    {/if}
  {:else if view.phase === "reviewing"}
    <p role="status">Loading review…</p>
  {:else if view.phase === "review_failed"}
    <p role="alert">{view.message}</p>
  {:else if view.phase === "review"}
    {@const review = view.review}
    {@const affected = review.affected_turns}
    <dl>
      <dt>Branch</dt><dd title={review.selection.branch}>{short(review.request.expected_head.branch)} → {short(review.selection.branch)}</dd>
      {#if affected != null}<dt>Turns affected</dt><dd>{affected}</dd>{/if}
      {#if review.request.active != null}<dt>Active run</dt><dd>Requires a separate stop</dd>{/if}
      {#if review.request.queued.length > 0}<dt>Queue</dt><dd>{count(review.request.queued.length, "message")} held during the operation</dd>{/if}
      {#if recoverable(review) > 0}<dt>Draft recovery</dt><dd>{count(recoverable(review), "message")} available after applying</dd>{/if}
    </dl>
    <p>Files and external changes stay as they are. The current branch is kept.</p>
    <button type="button" onclick={() => dispatch({ kind: "begin", request: review.request })}>Prepare {view.mode}</button>
  {:else}
    {@const effect = view.operation}
    {#if view.phase === "preparing"}
      <p role="status">{view.active ? "Stop the reviewed run to continue." : "No active run to stop."}</p>
      <button type="button" onclick={() => dispatch({ kind: "stop", operation: effect })}>{view.active ? "Stop run" : "Continue"}</button>
    {:else if view.phase === "stopping"}<p role="status">Waiting for the original run to stop…</p>
    {:else if view.phase === "ready"}
      <p role="status">Ready to switch branches.</p>
      <button type="button" onclick={() => dispatch({ kind: "apply", operation: effect })}>Apply {view.mode}</button>
    {:else if view.phase === "unknown"}
      <p role="status">{view.message}</p>
      <button type="button" onclick={() => dispatch({ kind: "check", operation: effect })}>Check status</button>
      {#if view.retry}<button type="button" onclick={() => dispatch({ kind: "retry_begin", operation: effect })}>Retry original operation</button>{/if}
    {:else if view.phase === "pending"}<p role="status">{view.label}</p>
    {:else if view.phase === "installing"}<p role="status">Loading the applied branch…</p>
    {:else if view.phase === "install_failed"}
      <p role="alert">{view.message}</p>
      <button type="button" onclick={() => dispatch({ kind: "check", operation: effect })}>Check branch</button>
    {:else if view.phase === "superseded"}<p role="status">A newer branch is selected.</p>
    {:else if view.phase === "applied"}
      <p role="status">Branch switched.</p>
      {#if view.recoverable.length > 0}
        <ul aria-label="Recoverable messages">
          {#each view.recoverable as input (input.id)}
            <li><span>{input.label}</span><button type="button" disabled={input.action === "blocked"} onclick={() => dispatch({ kind: "recover", operation: effect, head: view.head, input: input.id })}>{input.action === "return" ? "Return to draft" : input.action === "swap" ? "Swap drafts" : "Recover draft"}</button></li>
          {/each}
        </ul>
      {/if}
    {:else if view.phase === "aborted"}<p role="status">History operation ended. Any completed stop remains in effect.</p>{/if}
    {#if view.phase === "applied" || view.phase === "superseded" || view.phase === "aborted"}
      <button type="button" onclick={() => dispatch({ kind: "branches", operation: effect })}>Branches</button>
    {/if}
    {#if view.phase === "preparing" || view.phase === "stopping" || view.phase === "ready"}
      <button type="button" onclick={() => dispatch({ kind: "abort", operation: effect })}>Abort operation</button>
    {/if}
  {/if}
  {#if "fork" in view && view.fork}
    <button type="button" onclick={view.fork}>Fork this turn</button>
  {/if}
</section>
{/if}

<style>
  :global(.history-dialog) { max-block-size: min(80dvh, 38rem); overflow: auto; }
  .history-panel { display: grid; gap: .75rem; min-inline-size: 0; color: var(--fg); }
  header, li { display: flex; align-items: center; justify-content: space-between; gap: 1rem; }
  h2, p, ul, dl, dd { margin: 0; }
  h2 { font-size: 1rem; }
  ul { padding: 0; list-style: none; display: grid; gap: .3rem; }
  li { padding-block: .2rem; border-block-end: 1px solid var(--line); overflow-wrap: anywhere; }
  dl { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: .45rem .8rem; }
  dd { overflow-wrap: anywhere; }
  dt, .muted, p { color: var(--muted); }
  [role="alert"] { color: var(--danger); }
  button { justify-self: start; padding: .25rem .65rem; }
</style>
