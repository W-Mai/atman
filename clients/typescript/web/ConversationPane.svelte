<script lang="ts">
  import type { Part, Row, Unit } from "./live-projection.ts";

  type View = {
    rows: readonly Row[];
    placeholder: string;
    hasEarlier: boolean;
    loading: boolean;
  };

  let { loadEarlier, refresh, anchor }: {
    loadEarlier: () => void;
    refresh: () => void;
    anchor: (node: HTMLElement) => void;
  } = $props();

  let view: View = $state.raw({
    rows: [],
    placeholder: "Loading conversation",
    hasEarlier: false,
    loading: true,
  });

  export function update(next: View): void {
    view = next;
  }

  let overrides = $state.raw(new Map<string, { folded: boolean; open: boolean }>());

  function isOpen(row: Extract<Row, { kind: "work" }>): boolean {
    const override = overrides.get(row.key);
    return override?.folded === row.folded ? override.open : !row.folded;
  }

  function toggle(row: Extract<Row, { kind: "work" }>, event: MouseEvent & { currentTarget: HTMLButtonElement }) {
    const block = event.currentTarget.closest<HTMLElement>("[data-conversation-item]");
    if (block) anchor(block);
    const next = new Map(overrides);
    next.delete(row.key);
    next.set(row.key, { folded: row.folded, open: !isOpen(row) });
    if (next.size > 32) next.delete(next.keys().next().value!);
    overrides = next;
  }
</script>

{#snippet present(part: Part)}
  {#if part.kind === "text"}
    <p>{part.text}</p>
  {:else if part.kind === "quote"}
    <blockquote>{part.text}</blockquote>
  {:else if part.kind === "thinking"}
    <details class="thinking">
      <summary><span aria-hidden="true">⣿</span> Thinking</summary>
      <p>{part.text}</p>
    </details>
  {:else if part.kind === "tool_call"}
    <p class="entry-muted">{part.name}</p>
  {:else if part.kind === "tool_result"}
    <p class="entry-muted">{part.failed ? "Tool failed" : "Tool completed"}</p>
  {:else if part.kind === "image"}
    <p class="entry-muted">Image attachment</p>
  {/if}
{/snippet}

{#snippet units(parts: readonly Unit[])}
  {#each parts as unit (unit.key)}
    {@const first = unit.parts[0]}
    <div class="conversation-unit" data-unit={unit.key} data-origin={unit.origin}>
      {#if first?.kind === "tool_call"}
        <details class="tool-group">
          <summary>
            <span aria-hidden="true">⣿</span>
            {unit.parts.every((part) => part.kind === "tool_call" && part.name === first.name) ? first.name : "Tools"}
            {#if unit.parts.length > 1}<span class="tool-count">×{unit.parts.length}</span>{/if}
          </summary>
          {#each unit.parts as part}{@render present(part)}{/each}
        </details>
      {:else}
        {#each unit.parts as part}{@render present(part)}{/each}
      {/if}
      {#if unit.truncated}<small class="entry-muted">Preview clipped</small>{/if}
    </div>
  {/each}
{/snippet}

<section class="transcript" aria-label="Conversation" aria-busy={view.loading}>
  <div class="transcript-nav">
    {#if view.hasEarlier}
      <button type="button" class="earlier" onclick={loadEarlier} disabled={view.loading}>
        <span aria-hidden="true">↑</span> Earlier
      </button>
    {/if}
    <button type="button" class="refresh" aria-label="Refresh conversation" title="Refresh conversation"
      onclick={refresh} disabled={view.loading}>↻</button>
  </div>

  {#if view.rows.length === 0}
    <p class="transcript-placeholder">
      {#if view.loading}<span class="loading-mark" aria-hidden="true"></span>{/if}
      {view.placeholder}
    </p>
  {:else}
    <div class="transcript-items">
      {#each view.rows as row (row.key)}
        {#if row.kind === "work"}
          {@const open = isOpen(row)}
          <article data-conversation-item class="entry work" style:--work-duration={`${Math.min(700, 400 + row.units.length * 40)}ms`}>
            <button type="button" class="work-toggle" aria-expanded={open} onclick={(event) => toggle(row, event)}>
              <span aria-hidden="true">⣿</span> Work
              <span class="work-count" aria-hidden="true">{row.units.length}</span>
            </button>
            <div class="work-fold" class:open>
              <div class="work-clip" inert={!open}>
                <div class="work-body">{@render units(row.units)}</div>
              </div>
            </div>
          </article>
        {:else if row.kind === "final"}
          <article data-conversation-item class="entry final">{@render units(row.units)}</article>
        {:else}
          {@const body = row.item.body}
          {#if body.kind === "deferred"}
            <article data-conversation-item class="entry entry-muted">Message exceeds the browser page limit.</article>
          {:else if row.units.length > 0}
            <article data-conversation-item class="entry" class:authored={body.role === "user"}
              class:entry-muted={body.role === "system" || body.role === "tool"}>
              {@render units(row.units)}
            </article>
          {/if}
        {/if}
      {/each}
    </div>
  {/if}
</section>

<style>
  .transcript {
    max-inline-size: 62rem;
    min-block-size: 100%;
    margin-inline: auto;
    display: grid;
    grid-template-rows: auto 1fr;
  }

  .transcript-nav {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    min-block-size: 1.9rem;
    margin-block-end: 1rem;
  }

  .transcript-nav button {
    border: 0;
    min-block-size: 1.9rem;
    padding: 0.25rem 0.55rem;
    border-radius: 0;
    color: var(--muted);
    font: 0.82rem var(--atman-mono);
  }

  .transcript-nav button:not(:disabled):hover {
    color: var(--fg);
  }

  .refresh {
    position: absolute;
    inset-inline-end: 0;
  }

  .earlier span {
    margin-inline-end: 0.2ch;
  }

  .transcript-placeholder {
    align-self: center;
    justify-self: center;
    margin-block: 1.5rem 3rem;
    color: var(--muted);
    text-align: center;
    font: 0.85rem/1.6 var(--atman-mono);
  }

  .transcript-items {
    display: grid;
    align-content: start;
    gap: 1.125rem;
  }

  .entry {
    min-inline-size: 0;
    margin: 0;
    padding-inline: 0.8rem;
    font-size: 0.94rem;
    line-height: 1.65;
    overflow-wrap: anywhere;
    white-space: pre-wrap;
  }

  .entry p + p {
    margin-block-start: 0.5rem;
  }

  .conversation-unit + .conversation-unit {
    margin-block-start: 0.4rem;
  }

  .authored {
    padding-block: 0.75rem;
    background: var(--user-bg);
  }

  .entry-muted {
    color: var(--muted);
    font-size: 0.83rem;
  }

  .work {
    padding-inline: 0;
    border-inline: 1px dashed var(--line);
  }

  .work-toggle {
    display: flex;
    align-items: center;
    gap: 0.55rem;
    width: 100%;
    padding: 0.5rem 0.8rem;
    border: 0;
    border-radius: 0;
    color: var(--muted);
    text-align: start;
    font: 0.83rem/1.45 var(--atman-mono);
  }

  .work-toggle span:first-child { color: var(--accent); }
  .work-count { margin-inline-start: auto; color: var(--faint); }
  .work-fold {
    display: grid;
    grid-template-rows: 0fr;
    transition: grid-template-rows var(--work-duration, 400ms) cubic-bezier(.22, 1, .36, 1);
  }
  .work-fold.open { grid-template-rows: 1fr; }
  .work-clip { min-block-size: 0; overflow: hidden; }
  .work-body { padding: 0.25rem 0.8rem 0.6rem; }

  blockquote {
    margin: 0.35rem 0;
    padding-inline-start: 0.75rem;
    border-inline-start: 1px dashed var(--line);
    color: var(--muted);
  }

  .thinking,
  .tool-group {
    padding: 0.2rem 0.75rem;
    border-inline-start: 1px dashed var(--line);
    color: var(--muted);
  }

  .thinking summary,
  .tool-group summary {
    list-style: none;
    cursor: pointer;
    padding: 0.2rem 0.35rem;
    font: 0.83rem/1.45 var(--atman-mono);
  }

  .thinking summary::-webkit-details-marker,
  .tool-group summary::-webkit-details-marker {
    display: none;
  }

  .thinking summary:hover,
  .tool-group summary:hover {
    background: var(--hover);
  }

  .thinking summary span,
  .tool-group summary span:first-child {
    margin-inline-end: 0.25rem;
    color: var(--success, var(--accent));
  }

  .thinking p {
    margin-block: 0.35rem 0.55rem;
  }

  .tool-count { margin-inline-start: 0.35rem; color: var(--faint); }
</style>
