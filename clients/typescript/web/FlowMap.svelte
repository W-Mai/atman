<script lang="ts">
  import { executionPhase, flowMapRows, mapKey } from "./flow-map.ts";
  import type { FlowMapView } from "./flow-map.ts";

  let { entries, session, selected, pending, hasRelations, more, onOpen }: FlowMapView = $props();

  let loadingMore = $state(false);

  async function loadMore() {
    if (!more || loadingMore) return;
    loadingMore = true;
    try { await more(); }
    finally { loadingMore = false; }
  }

  const rows = $derived(flowMapRows(entries, session));
  const selectedKey = $derived(selected && mapKey(selected));
</script>

<!-- svelte-ignore a11y_no_noninteractive_tabindex (Focus enters the map before its node controls.) -->
<section class="flow-map" tabindex="-1" aria-label="Flow Map" aria-busy={pending || loadingMore || undefined}>
  {#if !hasRelations && rows.length > 0}
    <p class="map-note" role="status">Flow relationships unavailable</p>
  {/if}
  {#if rows.length}
    <ol class="map-tree">
      {#each rows as { key, entry, depth, stems, last, hasChildren } (key)}
        {@const label = entry.unavailable ? "Unavailable" : entry.archived ? "Archived" : entry.paused ? "Paused" : executionPhase[entry.phase ?? ""] ?? "Status unavailable"}
        <li class:hasChildren style:--map-depth={Math.min(depth, 13)}>
          <span class="connectors" aria-hidden="true">
            {#each stems as stem}
              <span class:stem></span>
            {/each}
            {#if depth}<span class="branch" class:last></span>{/if}
          </span>
          <button type="button" class="map-node" aria-pressed={key === selectedKey}
            disabled={pending || entry.unavailable} onclick={() => onOpen(entry.address)}>
            <span class="map-node-mark" aria-hidden="true"></span>
            <span class="map-name" title={entry.title}>{entry.title}</span>
            <span class="map-phase" data-phase={entry.phase} title={label}>{label}</span>
          </button>
        </li>
      {/each}
    </ol>
    {#if more}
      <button type="button" class="map-more" aria-disabled={pending || loadingMore} disabled={pending} onclick={loadMore}>Load more</button>
    {/if}
  {:else}
    <p class="map-note" role="status">{pending ? "Loading Flows…" : "No Flows"}</p>
  {/if}
</section>

<style>
  .flow-map { container-type: inline-size; min-inline-size: 0; overflow: auto; padding: .6rem 1rem 1rem; }
  .map-note { color: var(--muted); font-size: .78rem; margin: .3rem 0 .6rem; }
  .map-tree { margin: 0 0 .5rem; padding: 0; list-style: none; }
  li { display: flex; min-inline-size: 0; position: relative; }
  li.hasChildren::after { content: ""; position: absolute; pointer-events: none; inset-block: 50% 0; inset-inline-start: calc(var(--map-depth) * .75rem + .375rem); border-inline-start: 1px solid var(--muted); }
  .connectors { align-self: stretch; display: flex; flex: none; color: var(--muted); }
  .connectors > span { position: relative; inline-size: .75rem; }
  .stem::before, .branch::before, .branch::after { position: absolute; content: ""; }
  .stem::before, .branch::before { inset: 0 auto 0 50%; border-inline-start: 1px solid currentColor; }
  .branch.last::before { inset-block-end: 50%; }
  .branch::after { inset: 50% 0 auto 50%; border-block-start: 1px solid currentColor; }
  .map-node { display: grid; grid-template-columns: .55rem minmax(0, 1fr) auto; align-items: center; flex: 1; gap: .5rem; min-inline-size: 0; min-block-size: 2rem; padding: .25rem .55rem; text-align: start; border: 0; border-inline-start: 2px solid transparent; border-radius: 0; background: transparent; }
  .map-node:hover:not(:disabled) { background: var(--hover); }
  .map-node:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
  .map-node[aria-pressed="true"] { border-inline-start-color: var(--accent); background: var(--surface); }
  .map-node-mark { inline-size: .35rem; block-size: .35rem; border: 1px solid var(--muted); transform: rotate(45deg); justify-self: center; }
  .map-node[aria-pressed="true"] .map-node-mark { border-color: var(--accent); background: var(--accent); }
  .map-name, .map-phase { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .map-phase { color: var(--muted); font-size: .75rem; }
  .map-phase[data-phase="running"], .map-phase[data-phase="finishing"] { color: var(--accent); }
  .map-phase[data-phase="interrupted"] { color: var(--danger); }
  .map-more { padding: .25rem .5rem; }
  @container (max-width: 33rem) {
    .connectors, li.hasChildren::after { display: none; }
    .map-node { grid-template-columns: .55rem minmax(0, 1fr) minmax(0, max-content); }
  }
</style>
