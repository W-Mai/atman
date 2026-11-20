<script lang="ts">
  import { tick } from "svelte";
  import { cursorId } from "../src/index.ts";
  import type { Dtos } from "../src/index.ts";
  import { inboxLane } from "./inbox.ts";
  import type { InboxAction, InboxItem, InboxPage, InboxView } from "./inbox.ts";

  let { view, dispatch }: { view: InboxView; dispatch: (action: InboxAction) => void } = $props();
  type Change = Dtos["FlowInboxChange"];
  let expanded = $state(true);
  let editor = $state<HTMLTextAreaElement>();
  let editing = $state<{ id: string; revision: string; text: string }>();
  let header: HTMLButtonElement | undefined;
  let trigger: HTMLButtonElement | undefined;
  const encoder = new TextEncoder();
  const current = (item: InboxItem) => editing?.id === item.id && editing.revision === revision(item);
  // The wire receipt supplies the full identity; the component does not infer it from row order.
  const revision = ({ revision }: InboxItem): string => cursorId(revision);

  function act(page: InboxPage, item: InboxItem, change: Change): void {
    dispatch({ kind: "edit", page, item, change });
  }

  async function edit(item: InboxItem, button: HTMLButtonElement): Promise<void> {
    trigger = button;
    editing = { id: item.id, revision: revision(item), text: item.text };
    await tick();
    editor?.focus();
  }

  function closeEditor(): void {
    editing = undefined;
    if (trigger?.isConnected && !trigger.disabled) trigger.focus();
    else header?.focus();
  }

  function save(page: InboxPage, item: InboxItem): void {
    if (!editing || !current(item) || !view.writable) return;
    act(page, item, { action: "edit_text", text: editing.text });
  }
</script>

<section class="queue" aria-label="Input queue">
  <header>
    <button type="button" class="queue-heading" bind:this={header} aria-expanded={expanded}
      aria-label="Queue entries" onclick={() => { if (expanded) editing = undefined; expanded = !expanded; }}>
      <span aria-hidden="true">≡</span> Queue <span class="count">{view.page?.total ?? ""}</span>
    </button>
    {#if view.page && view.page.total > 16}
      {@const page = view.page}
      <nav aria-label="Queue pages">
        <button type="button" title="Previous queue page" aria-label="Previous queue page" disabled={page.offset === 0}
          onclick={() => dispatch({ kind: "page", offset: page.offset - 16 })}>‹</button>
        <span>{page.offset + 1}–{page.offset + page.items.length}</span>
        <button type="button" title="Next queue page" aria-label="Next queue page" disabled={page.offset + 16 >= page.total}
          onclick={() => dispatch({ kind: "page", offset: page.offset + 16 })}>›</button>
      </nav>
    {/if}
  </header>
  {#if expanded && view.page?.total}
    {@const page = view.page}
    <ul>
      {#each page.items as item (item.id)}
        {@const lane = inboxLane(page, item)}
        {@const index = lane.indexOf(item.id)}
        {@const inserted = item.delivery === "at_boundary"}
        {@const move = view.writable && !item.held && !inserted && index >= 0}
        <li>
          <div class="queue-row" class:inserted>
            <span class="kind" title={item.held ? "Held for another branch" : inserted ? "Inserted into the current turn" : "Queued for the next turn"}
              aria-label={item.held ? "Held" : inserted ? "Inserted" : "Next turn"} role="img">{item.held ? "‖" : inserted ? "↳" : "≡"}</span>
            <span class="preview" title={item.text}>{item.text || "Attachment"}{item.text_truncated ? "…" : ""}</span>
            {#if item.has_quote}<span class="meta" title="Includes a quote">❞</span>{/if}
            {#if item.image_count}<span class="meta" title="Attached images">▧{item.image_count}</span>{/if}
            {#if view.editable}
              <div class="queue-actions" role="group" aria-label="Queued input actions">
                {#if !item.held}
                  <button type="button" title="Edit text" aria-label="Edit queued text" disabled={!view.writable || item.text_truncated}
                    onclick={(event) => void edit(item, event.currentTarget)}>✎</button>
                  {#if inserted || !item.separate_turn}
                    <button type="button" title={inserted ? "Undo insert" : "Insert into the current turn"}
                      aria-label={inserted ? "Undo insert" : "Insert into the current turn"}
                      disabled={!view.writable || (!inserted && !page.insertion_available)}
                      onclick={() => act(page, item, { action: "set_delivery", delivery: inserted ? "next_turn" : "at_boundary" })}>{inserted ? "↶" : "↳"}</button>
                  {/if}
                  {#if !inserted}
                    <button type="button" title="Move earlier" aria-label="Move earlier" disabled={!move || index === 0}
                      onclick={() => act(page, item, { action: "move", direction: "earlier", expected_order: lane })}>↑</button>
                    <button type="button" title="Move later" aria-label="Move later" disabled={!move || index >= lane.length - 1}
                      onclick={() => act(page, item, { action: "move", direction: "later", expected_order: lane })}>↓</button>
                  {/if}
                {/if}
                <button type="button" title="Cancel queued input" aria-label="Cancel queued input" disabled={!view.writable}
                  onclick={() => act(page, item, { action: "cancel" })}>×</button>
              </div>
            {/if}
          </div>
          {#if editing && current(item)}
            <form class="queue-editor" onsubmit={(event) => { event.preventDefault(); save(page, item); }}>
              <textarea rows="2" aria-label="Edit queued input" bind:this={editor} bind:value={editing.text}
                readonly={view.busy || view.retry} onkeydown={(event) => {
                  if (event.key === "Escape" && !event.isComposing) { event.stopPropagation(); closeEditor(); }
                }}></textarea>
              <div>
                <button type="button" onclick={closeEditor}>Close</button>
                <button type="submit" disabled={!view.writable || editing.text === item.text || encoder.encode(editing.text).length > 65536}>Save</button>
              </div>
            </form>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
  {#if view.feedback || view.busy}
    <div class="feedback"><span role="status">{view.busy ? "Saving queue change…" : view.feedback}</span>
      {#if view.retry}<button type="button" onclick={() => dispatch({ kind: "retry" })}>Retry original change</button>
      {:else if !view.page && !view.busy}<button type="button" onclick={() => dispatch({ kind: "page", offset: 0 })}>Refresh queue</button>{/if}
    </div>
  {/if}
</section>

<style>
  .queue { max-inline-size: 80ch; margin: 0 auto .45rem; font: .78rem/1.4 var(--atman-mono); color: var(--muted); }
  header, nav, .queue-row, .queue-actions, .feedback { display: flex; align-items: center; }
  header { justify-content: space-between; border-bottom: 1px solid var(--line); }
  nav { gap: .3rem; }
  ul { max-block-size: min(10rem, 25dvh); overflow: auto; padding: 0; margin: 0; list-style: none; }
  .queue-row { gap: .35rem; min-block-size: 1.65rem; padding-inline-start: .42rem; }
  .kind { flex: 0 0 1ch; color: var(--muted); }
  .inserted .kind, .inserted .preview { color: var(--accent); }
  .preview { flex: 1; min-inline-size: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .meta, .count { color: var(--muted); }
  .queue-actions { flex: none; }
  button { border: 0; border-radius: 0; background: transparent; color: inherit; font: inherit; padding: .23rem .45rem; }
  button:disabled { opacity: .55; }
  button:not(:disabled):hover, button:focus-visible { background: var(--hover); color: var(--fg); }
  .queue-heading { color: var(--fg); }
  .queue-editor { padding: .25rem .45rem .4rem; }
  .queue-editor textarea { display: block; width: 100%; border: 1px solid var(--line); padding: .35rem .45rem; resize: vertical;
    color: var(--fg); background: var(--bg); font: inherit; }
  .queue-editor div { display: flex; justify-content: end; }
  .feedback { gap: .5rem; padding-inline: .45rem; flex-wrap: wrap; overflow-wrap: anywhere; }
</style>
