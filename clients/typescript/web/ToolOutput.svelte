<script lang="ts">
  import { onDestroy } from "svelte";
  import { ClientError, type Dtos } from "../src/index.ts";
  import type { ArtifactTarget, ReadArtifact } from "./tool-output.ts";

  type Page = Extract<Dtos["ReadConversationArtifactResponse"], { state: "ready" }>;
  type Notice = { text: string; retry: boolean };

  let { part, target, read, active, visible, anchor }: {
    part: Extract<Dtos["ConversationPart"], { kind: "tool_result" }>;
    target: ArtifactTarget | undefined;
    read: ReadArtifact | undefined;
    active: boolean;
    visible: boolean;
    anchor: (node: HTMLElement) => void;
  } = $props();

  let open = $state(false);
  let full = $state(false);
  let page = $state.raw<Page>();
  let notice = $state.raw<Notice>();
  let pending = $state.raw<AbortController>();
  let region = $state<HTMLDivElement>();
  let heading = $state<HTMLElement>();
  let modeButton = $state<HTMLButtonElement>();
  let source: ArtifactTarget | undefined;
  let offsets = [0];
  let position = $state(0);
  let failed = $state.raw<{ index: number; offset: number }>();
  const targetKey = $derived(target?.key);
  const number = new Intl.NumberFormat();

  function stop(): void {
    pending?.abort();
    pending = undefined;
  }

  function reset(): void {
    stop();
    full = false;
    page = undefined;
    notice = undefined;
    source = undefined;
    failed = undefined;
    offsets = [0];
    position = 0;
  }

  $effect(() => {
    if (open && visible && targetKey !== undefined) return reset;
  });
  $effect(() => {
    if (!active && pending) {
      stop();
      notice = { text: "Reading paused.", retry: true };
    }
  });
  onDestroy(stop);

  function describe(result: Exclude<Dtos["ReadConversationArtifactResponse"], Page>): Notice {
    switch (result.state) {
      case "verification_budget":
        return { text: `Output exceeds the ${number.format(result.allowed / (1024 * 1024))} MiB verification limit.`, retry: false };
      case "capacity":
        return result.kind === "readers_busy"
          ? { text: "Output readers are busy.", retry: true }
          : { text: "Conversation read limit reached.", retry: false };
      case "source_changed": return { text: "Conversation changed. Refresh to read this output again.", retry: false };
      case "content_changed": return { text: "Stored output changed.", retry: true };
      case "invalid_range": return { text: "This output position is unavailable.", retry: false };
      case "unavailable": return { text: "Output is unavailable.", retry: true };
    }
  }

  async function load(index: number, offset: number): Promise<void> {
    if (!open || !visible || !active || pending || !target || !read || !heading) return;
    const captured = source ?? target;
    const controller = new AbortController();
    const current = () => pending === controller && !controller.signal.aborted &&
      open && visible && active && targetKey === captured.key;
    anchor(heading);
    source = captured;
    pending = controller;
    full = true;
    notice = undefined;
    failed = { index, offset };
    try {
      const result = await read(captured, offset, controller.signal);
      if (!current()) return;
      if (result.state !== "ready") {
        notice = describe(result);
        return;
      }
      if (result.total_bytes !== captured.bytes) {
        notice = { text: "Output size differs from its registration.", retry: false };
        return;
      }
      position = index;
      if (offsets[index] !== offset) offsets = [...offsets.slice(0, index), offset];
      page = result;
      failed = undefined;
      if (region) region.scrollTop = 0;
    } catch (error) {
      if (!current()) return;
      notice = error instanceof ClientError && error.kind === "capability"
        ? { text: "Full output is unavailable on this host.", retry: false }
        : { text: "Could not read output.", retry: true };
    } finally {
      if (pending === controller) pending = undefined;
    }
  }

  function showPreview(): void {
    if (heading) anchor(heading);
    reset();
  }
</script>

{#if part.preview || target}
  <details class="tool-preview" bind:open>
    <summary bind:this={heading} onclick={(event) => anchor(event.currentTarget)}>
      {part.failed ? "Tool failed" : "Output"}
      {#if part.preview_truncated}
        <span class="clipped" title="Preview clipped" aria-hidden="true">…</span>
        <span class="visually-hidden"> (preview clipped)</span>
      {/if}
    </summary>
    {#if target}
      <div class="tool-output-bar">
        <button bind:this={modeButton} type="button" aria-disabled={!full && !active}
          onclick={() => { if (full) showPreview(); else void load(0, 0); }}>{full ? "Preview" : "Full output"}</button>
        {#if full}
          <span role="status">
            {#if pending}Reading…
            {:else if page}{number.format(page.offset + Number(page.content.length > 0))}–{number.format(page.next_offset)} / {number.format(page.total_bytes)} B
            {/if}
          </span>
          <button type="button" aria-label="Previous output page" title="Previous page"
            aria-disabled={!active || !!pending || position === 0}
            onclick={() => { if (position > 0) void load(position - 1, offsets[position - 1]!); }}>‹</button>
          <button type="button" aria-label="Next output page" title="Next page"
            aria-disabled={!active || !!pending || !page?.has_more}
            onclick={() => { if (page?.has_more) void load(position + 1, page.next_offset); }}>›</button>
        {:else}
          <span>{number.format(target.bytes)} B</span>
        {/if}
      </div>
    {/if}
    {#if full && notice}
      <div class="tool-output-notice">
        <span role="status">{notice.text}</span>
        {#if notice.retry && failed}
          <button type="button" aria-disabled={!active || !!pending}
            onclick={() => {
              if (!active || pending || !failed) return;
              modeButton?.focus({ preventScroll: true });
              void load(failed.index, failed.offset);
            }}>Retry</button>
        {/if}
      </div>
    {/if}
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (The bounded output region supports keyboard scrolling.) -->
    <div bind:this={region} class="tool-preview-text" class:full role="region"
      aria-label="Tool output" aria-busy={pending !== undefined} tabindex="0">
      <pre>{full ? page?.content ?? part.preview : part.preview}</pre>
    </div>
  </details>
{:else}
  <p class="clipped">{part.failed ? "Tool failed" : "Tool completed"}</p>
{/if}

<style>
  .clipped { color: var(--muted); font-size: 0.83rem; }
  .tool-preview {
    padding: 0.2rem 0.75rem;
    border-inline-start: 1px dashed var(--line);
    color: var(--muted);
  }
  summary {
    cursor: pointer;
    padding: 0.2rem 0.35rem;
    font: 0.83rem/1.45 var(--atman-mono);
  }
  summary:hover { background: var(--hover); }
  .tool-output-bar, .tool-output-notice {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.25rem 0.45rem;
    font: 0.75rem/1.4 var(--atman-mono);
  }
  .tool-output-bar > span { flex: 1; }
  .tool-output-notice { color: var(--warn); padding-block: 0.25rem; }
  button {
    min-block-size: 1.65rem;
    padding-inline: 0.55rem;
    border: 1px solid transparent;
    color: var(--fg);
    background: var(--surface);
    font: inherit;
    cursor: pointer;
  }
  button[aria-disabled="true"] { opacity: 0.5; cursor: default; }
  button:not([aria-disabled="true"]):is(:hover, :focus-visible) { border-color: var(--accent); }
  .tool-preview-text {
    box-sizing: border-box;
    max-block-size: 14rem;
    overflow: auto;
    overscroll-behavior: contain;
    margin-block: 0.2rem 0.4rem;
    padding: 0.55rem 0.65rem;
    border: 1px solid var(--line);
    background: var(--surface);
    color: var(--fg);
  }
  .tool-preview-text.full { block-size: 14rem; }
  .tool-preview-text:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  pre {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    font: 0.83rem/1.5 var(--atman-mono);
  }
</style>
