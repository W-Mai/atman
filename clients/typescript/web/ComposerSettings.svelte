<script lang="ts">
  import { onDestroy, onMount, untrack } from "svelte";
  import type { Dtos } from "../src/index.ts";
  import { reasoningLabel } from "./composer-settings.ts";
  import type { PreferenceEditorView } from "./preference-editor.ts";

  type Snapshot = Dtos["FlowComposerResponse"];
  type Origin = Dtos["ComposerPreferenceOrigin"];
  type Row = { name: string; value: string; origin?: Origin };

  type Edit = Dtos["ComposerPreferenceEdit"];
  type Section = Edit["field"];
  let { settings, browse, close, section, preference, edit, check, retry }: {
    settings: Snapshot;
    browse: (query: string, signal: AbortSignal) => Promise<Snapshot | undefined>;
    close: () => void;
    section: Section;
    preference: PreferenceEditorView | undefined;
    edit: (revision: string, value: Edit) => void;
    check: () => void;
    retry: () => void;
  } = $props();

  const titleId = $props.id();
  let dialog: HTMLDialogElement;
  let search = $state<HTMLInputElement>();
  let query = $state("");
  let phase = $state<"loading" | "ready" | "unavailable" | "failed" | "too_long">("loading");
  let observed = $state.raw<Snapshot>();
  let catalog = $state.raw<Dtos["ComposerModelCatalog"]>();
  let request: AbortController | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const encoder = new TextEncoder();
  let details = $derived(fields(settings));
  let locked = $derived(!preference || preference.busy || preference.unsettled);
  const headings: Record<Section, string> = {
    model: "Browse Models", reasoning: "Reasoning", trust_mode: "Permissions", escalation: "Escalation",
  };

  onMount(() => {
    dialog.showModal();
    search?.focus();
  });

  let previousModelRevision: string | undefined;
  let initialized = false;
  $effect(() => {
    if (section !== "model") return;
    const modelRevision = settings.edit_revisions?.model;
    if (initialized && modelRevision === previousModelRevision) return;
    initialized = true;
    previousModelRevision = modelRevision;
    untrack(() => schedule(query, 0));
  });

  onDestroy(retire);

  function retire(): void {
    clearTimeout(timer);
    timer = undefined;
    request?.abort();
    request = undefined;
  }

  function dismiss(): void {
    retire();
    dialog.close();
    close();
  }

  function schedule(value: string, delay = 160): void {
    retire();
    query = value;
    catalog = undefined;
    const term = value.trim();
    if (encoder.encode(term).byteLength > 256) {
      phase = "too_long";
      return;
    }
    phase = "loading";
    const next = new AbortController();
    request = next;
    timer = setTimeout(async () => {
      timer = undefined;
      try {
        const result = await browse(term, next.signal);
        if (request !== next || next.signal.aborted) return;
        if (result) observed = result;
        catalog = result?.catalog ?? undefined;
        phase = catalog ? "ready" : "unavailable";
      } catch {
        if (request === next && !next.signal.aborted) phase = "failed";
      }
    }, delay);
  }

  function fields(snapshot: Snapshot): Row[] {
    const { model, trust, origins } = snapshot;
    const reasoning = model.status === "ready" ? model.reasoning : undefined;
    const selected = reasoning?.requested ?? reasoning?.configured;
    const effective = reasoning?.effective;
    const chosen = selected ? reasoningLabel(selected) : "Unavailable";
    const resolved = effective ? reasoningLabel(effective) : chosen;
    const rows: Row[] = [
      { name: "Model", value: model.status === "ready" ? model.model : model.status === "pending" ? "Pending" : "Unavailable", ...(origins ? { origin: origins.model } : {}) },
      { name: "Reasoning", value: reasoning?.available ? (resolved === chosen ? chosen : `${chosen} · effective ${resolved}`) : "Unavailable", ...(origins ? { origin: origins.reasoning } : {}) },
      { name: "Permissions", value: `${trust.icon} ${trust.name}`, ...(origins ? { origin: origins.trust_mode } : {}) },
    ];
    if (trust.mode === "eager" && trust.escalation) {
      rows.push({ name: "Escalation", value: `${trust.escalation.icon} ${trust.escalation.name}`, ...(origins ? { origin: origins.escalation } : {}) });
    }
    return rows;
  }

  function originLabel(origin: Origin | undefined): string {
    return origin === "global" ? "Inherited" : origin === "session" ? "For this Session" : "Source unavailable";
  }

  function choose(value: Edit, snapshot = settings): void {
    const revision = snapshot.edit_revisions?.[value.field];
    if (!locked && revision && revision === settings.edit_revisions?.[value.field]) edit(revision, value);
  }
</script>

<dialog class="model-browser" bind:this={dialog} aria-labelledby={titleId}
  oncancel={(event) => { event.preventDefault(); dismiss(); }}>
  <header class="dialog-heading">
    <h2 id={titleId}>{headings[section]}</h2>
    <button type="button" class="close" aria-label="Close settings" onclick={dismiss}>×</button>
  </header>

  <dl class="settings" aria-label="Current settings">
    {#each details as field (field.name)}
      <div class="setting">
        <dt>{field.name}</dt>
        <dd class="value">{field.value}</dd>
        <dd class="origin" class:local={field.origin === "session"}>{originLabel(field.origin)}</dd>
      </div>
    {/each}
  </dl>

  {#if section === "model"}
    <input type="search" bind:this={search} value={query} aria-label="Search models" placeholder="Search models or providers"
    oninput={(event) => schedule(event.currentTarget.value)}
    onkeydown={(event) => {
      if (event.key === "Escape" && !event.isComposing) {
        event.preventDefault();
        dismiss();
      }
    }} />
  <div class="results" aria-busy={phase === "loading"}>
    {#if phase === "loading"}
      <p class="result-status" role="status">Loading models…</p>
    {:else if phase === "too_long"}
      <p class="result-status" role="status">Search is limited to 256 bytes.</p>
    {:else if phase === "failed"}
      <p class="result-status" role="status">Could not load models. <button type="button" onclick={() => schedule(query, 0)}>Retry</button></p>
    {:else if phase === "unavailable"}
      <p class="result-status" role="status">Model directory unavailable for this target.</p>
    {:else if !catalog?.rows.length}
      <p class="result-status" role="status">No matching models.</p>
    {:else}
      <ul aria-label="Model directory">
        {#each catalog.rows as row (JSON.stringify([row.alias, row.key]))}
          <li>
            {#if preference && observed?.edit_revisions}
              {@const displayed = observed}
              <button type="button" class="model-option" disabled={locked || displayed.edit_revisions?.model !== settings.edit_revisions?.model}
                onclick={() => choose({ field: "model", key: row.key }, displayed)}>
                <span class="model-key">{row.key}</span>
                <span class="model-source">{row.alias ? `${row.model} · ` : ""}{row.provider}</span>
              </button>
            {:else}
              <div class="model-option">
                <span class="model-key">{row.key}</span>
                <span class="model-source">{row.alias ? `${row.model} · ` : ""}{row.provider}</span>
              </div>
            {/if}
          </li>
        {/each}
      </ul>
      {#if catalog.truncated}
        <p class="result-status" role="status">More matches available. Narrow the search.</p>
      {/if}
    {/if}
  </div>
  {:else if section === "reasoning" && settings.model.status === "ready"}
    {@const reasoning = settings.model.reasoning}
    <div class="choices" aria-label="Reasoning choices">
      <button type="button" disabled={locked} aria-pressed={reasoning.requested == null}
        onclick={() => choose({ field: "reasoning", value: { kind: "model_default" } })}>Model default</button>
      {#each reasoning.choices as choice (JSON.stringify(choice))}
        <button type="button" disabled={locked} aria-pressed={JSON.stringify(choice) === JSON.stringify(reasoning.requested)}
          onclick={() => choose({ field: "reasoning", value: { kind: "selection", selection: choice } })}>{reasoningLabel(choice)}</button>
      {/each}
    </div>
  {:else if section === "trust_mode"}
    <div class="choices" aria-label="Permission modes">
      {#each settings.trust_choices?.modes ?? [] as mode (mode.mode)}
        <button type="button" disabled={locked} title={mode.description} aria-pressed={mode.mode === settings.trust.mode}
          onclick={() => choose({ field: "trust_mode", value: mode.mode })}>{mode.icon} {mode.name}</button>
      {/each}
    </div>
  {:else if section === "escalation" && settings.trust.mode === "eager"}
    <div class="choices" aria-label="Escalation policies">
      {#each settings.trust_choices?.escalations ?? [] as escalation (escalation.policy)}
        <button type="button" disabled={locked} aria-pressed={escalation.policy === settings.trust.escalation?.policy}
          onclick={() => choose({ field: "escalation", value: escalation.policy })}>{escalation.icon} {escalation.name}</button>
      {/each}
    </div>
  {:else}
    <p role="status" class="result-status">This setting is unavailable.</p>
  {/if}
  {#if preference?.message}
    <div class="preference-state">
      <span role="status">{preference.message}</span>
      {#if preference.unsettled && !preference.busy}<button type="button" onclick={check}>Check status</button>{/if}
      {#if preference.retryable}<button type="button" onclick={retry}>Retry original change</button>{/if}
    </div>
  {/if}
</dialog>

<style>
  .model-browser {
    inline-size: min(34rem, calc(100vw - 2rem));
    max-block-size: min(32rem, calc(100dvh - 2rem));
    padding: 1rem;
    overflow: auto;
    border-radius: 0;
    font: 0.85rem/1.4 var(--atman-mono);
  }

  .model-browser[open] { display: grid; gap: 0.7rem; }
  .dialog-heading h2 { margin: 0; font-size: 0.95rem; }
  .close { padding: 0.15rem 0.55rem; }
  .settings { display: grid; gap: 0.35rem; border-block: 1px solid var(--line); padding-block: 0.65rem; margin: 0; }
  .setting { display: grid; grid-template-columns: minmax(0, 7.5rem) minmax(0, 1fr) max-content; gap: 0.45rem; align-items: baseline; }
  .setting dd { margin: 0; }
  .setting dt, .origin, .model-source, .result-status { color: var(--muted); }
  .value, .model-key, .model-source { overflow-wrap: anywhere; }
  .origin { font-size: 0.75em; }
  .origin.local { color: var(--accent); }
  input { min-inline-size: 0; inline-size: 100%; }
  .results { block-size: min(13rem, 27dvh); overflow: auto; scrollbar-gutter: stable; }
  .results ul { list-style: none; margin: 0; padding: 0; }
  .model-option { inline-size: 100%; display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: 0.5rem; padding: 0.36rem 0.45rem; border: 0; border-block-end: 1px solid var(--line); text-align: start; font: inherit; }
  .model-source { text-align: end; }
  .result-status { margin: 0; padding: 0.4rem 0.45rem; }
  .choices, .preference-state { display: flex; flex-wrap: wrap; gap: .35rem; align-items: center; }
  .choices button, .preference-state button { padding: .25rem .5rem; font: inherit; }
  .choices button[aria-pressed="true"] { color: var(--accent); }

  @media (max-width: 440px) {
    .setting { grid-template-columns: minmax(0, 1fr) max-content; }
    .setting .value { grid-column: 1 / -1; grid-row: 2; }
    .setting { grid-template-rows: auto auto; row-gap: 0.05rem; }
  }
</style>
