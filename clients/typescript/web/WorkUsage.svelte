<script lang="ts">
  import { untrack } from "svelte";
  import type { Dtos } from "../src/index.ts";
  import type { UsageObservation } from "./work-usage.ts";

  let { observation, visible, latest = false, anchor }: {
    observation: UsageObservation;
    visible: boolean;
    latest?: boolean;
    anchor?: (node: HTMLElement) => void;
  } = $props();

  type Report = Extract<Dtos["ReadConversationUsageResponse"], { state: "ready" }>;
  const number = new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 });
  const percent = new Intl.NumberFormat(undefined, { style: "percent", maximumFractionDigits: 1 });
  const provenance: Record<Dtos["ConversationUsageSource"], string> = {
    provider: "Reported", mixed: "Includes estimates", estimated: "Estimated", unknown: "Source unknown",
  };
  let open = $state(false);
  let pending = $state.raw<AbortController>();
  let report = $state.raw<Report>();
  let notice = $state("");
  const key = $derived(observation.key);

  $effect(() => {
    if (!open || !visible || !key) return;
    untrack(() => { void refresh(); });
    return () => {
      pending?.abort();
      pending = undefined;
      notice = "";
    };
  });

  async function refresh(): Promise<void> {
    if (!open || !visible || pending) return;
    const selected = observation;
    const controller = new AbortController();
    const current = () => pending === controller && !controller.signal.aborted && open && visible && key === selected.key;
    pending = controller;
    notice = "";
    try {
      const result = await selected.read(selected.locator, controller.signal);
      if (!current()) return;
      switch (result.state) {
        case "ready": report = result; break;
        case "source_changed": notice = "Conversation changed. Refresh usage."; break;
        case "capacity": notice = result.kind === "readers_busy" ? "Usage readers are busy." : "Usage read limit reached."; break;
        case "unavailable": notice = "Usage unavailable."; break;
      }
    } catch {
      if (current()) notice = "Could not read usage.";
    } finally {
      if (pending === controller) pending = undefined;
    }
  }

  function routeLabel(value: string): string { return value.replaceAll("_", " "); }
</script>

{#snippet metric(symbol: string, label: string, value: number)}
  <div title={label}>
    <dt><span aria-hidden="true">{symbol}</span><span class="metric-name">{label}</span></dt>
    <dd>{number.format(value)}</dd>
  </div>
{/snippet}

<details class="usage" class:latest bind:open>
  <summary onclick={(event) => anchor?.(event.currentTarget)}>
    <span aria-hidden="true">≋</span> {latest ? "Latest turn usage" : "Usage"}
  </summary>
  {#if open && (visible || !latest)}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (Allow keyboard scrolling of the bounded report.) -->
    <section class="usage-body" aria-label="Usage details" aria-busy={!!pending} tabindex="0">
      <div class="usage-header">
        <span role="status">{pending ? "Reading usage…" : notice}</span>
        <button type="button" aria-label="Refresh usage" title="Refresh usage" aria-disabled={!!pending}
          onclick={() => { void refresh(); }}>↻</button>
      </div>
      {#if report}
        {#if report.edits.applied}
          <dl class="metrics edits" aria-label="Applied edits">
            {@render metric("+", "Added lines", report.edits.insertions)}
            {@render metric("−", "Removed lines", report.edits.deletions)}
            {@render metric("▤", "Edited files", report.edits.files)}
            {@render metric("±", "Hunks", report.edits.hunks)}
          </dl>
        {/if}
        {#each report.routes as route (JSON.stringify([route.provider, route.model, route.purpose, route.scope, route.source]))}
          <div class="route">
            <div class="route-title"><strong>{route.model}</strong><span>{provenance[route.source]}</span></div>
            <p class="route-scope">{[route.provider, route.scope, route.purpose].filter((name): name is string => name !== null).map(routeLabel).join(" · ")}</p>
            <dl class="metrics" aria-label="Token usage">
              {#if route.source !== "unknown" || route.input}{@render metric("↑", "Input tokens", route.input)}{/if}
              {#if route.source !== "unknown" || route.output}{@render metric("↓", "Output tokens", route.output)}{/if}
              {#if route.cache_read > 0}
                {@render metric("↻", "Cache read tokens", route.cache_read)}
              {/if}
              {#if route.cache_write > 0}
                {@render metric("⇥", "Cache write tokens", route.cache_write)}
              {/if}
              {#if route.reasoning}{@render metric("◌", "Reasoning tokens", route.reasoning)}{/if}
            </dl>
            <p class="route-scope">
              {number.format(route.calls)} {route.calls === 1 ? "call" : "calls"}
              {#if route.failures} · {number.format(route.failures)} failed{/if}
              {#if route.source === "provider" && route.cache_read_calls === route.calls && route.input > 0}
                · {percent.format(route.cache_read / route.input)} cache reuse
              {/if}
            </p>
          </div>
        {/each}
        {#if report.interrupted_calls}
          <p class="route-scope">{number.format(report.interrupted_calls)} interrupted</p>
        {:else if report.routes.length === 0 && report.edits.applied === 0}
          <p class="route-scope">No recorded usage or edits.</p>
        {/if}
      {/if}
    </section>
  {/if}
</details>

<style>
  .usage { min-inline-size: 0; padding: 0.2rem 0.75rem; border-inline-start: 1px dashed var(--line); }
  .usage.latest { margin: 0.2rem 0.55rem 0.7rem; padding-inline: 0; border: 0; }
  summary { padding: 0.24rem 0.35rem; cursor: pointer; color: var(--muted); font: 0.83rem/1.45 var(--atman-mono); }
  summary:hover { background: var(--hover); color: var(--fg); }
  summary span { color: var(--accent); margin-inline-end: 0.3rem; }
  .usage-body { min-block-size: 3rem; max-block-size: min(19rem, 48dvh); overflow: auto; overscroll-behavior: contain;
    padding: 0.35rem 0.55rem 0.55rem; background: var(--surface); border: 1px solid var(--line); }
  .usage-header { display: flex; justify-content: space-between; align-items: center; gap: 0.35rem;
    color: var(--muted); font: 0.75rem/1.4 var(--atman-mono); }
  .usage-header button { padding: 0.17rem 0.5rem; }
  button[aria-disabled="true"] { opacity: 0.5; cursor: default; }
  .metrics { display: flex; flex-wrap: wrap; gap: 0.25rem 0.65rem; margin: 0.35rem 0;
    font: 0.8rem/1.55 var(--atman-mono); font-variant-numeric: tabular-nums; }
  .metrics div { display: flex; min-inline-size: 6ch; gap: 0.3rem; }
  .metrics dt { position: relative; color: var(--muted); }
  .metrics dd { margin: 0; min-inline-size: 4ch; }
  .metric-name { position: absolute; inline-size: 1px; block-size: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
  .edits dt { color: var(--accent); }
  .edits div:nth-child(2) dt { color: var(--danger); }
  .route { padding-block: 0.35rem; border-block-start: 1px solid var(--line); }
  .route-title { display: flex; justify-content: space-between; align-items: baseline; flex-wrap: wrap; gap: 0.2rem 0.65rem;
    font: 0.79rem/1.45 var(--atman-mono); overflow-wrap: anywhere; }
  .route-title span, .route-scope { color: var(--muted); font-size: 0.74rem; }
  .route-scope { margin: 0.12rem 0; overflow-wrap: anywhere; }
</style>
