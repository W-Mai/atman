<script lang="ts">
  import { untrack } from "svelte";
  import type { Dtos } from "../src/index.ts";
  import type { DiffTarget, ReadDiff } from "./work-diff.ts";

  type Page = Extract<Dtos["ReadConversationDiffResponse"], { state: "ready" }>;
  let { target, read, active, visible, anchor }: {
    target: DiffTarget;
    read: ReadDiff;
    active: boolean;
    visible: boolean;
    anchor: (node: HTMLElement) => void;
  } = $props();

  const limit = 64;
  let open = $state(false);
  let pending = $state.raw<AbortController>();
  let page = $state.raw<Page>();
  let retryOffset = $state<number>();
  let notice = $state("");
  let header = $state<HTMLElement>();
  let region = $state<HTMLDivElement>();
  const key = $derived(target.key);

  $effect(() => {
    if (!open || !active || !visible || !key) return;
    untrack(() => { void load(0); });
    return () => {
      pending?.abort();
      pending = undefined;
      page = undefined;
      notice = "";
      retryOffset = undefined;
    };
  });

  async function load(offset: number): Promise<void> {
    if (pending || !open || !active || !visible) return;
    const controller = new AbortController();
    const selected = target;
    const current = () => pending === controller && !controller.signal.aborted &&
      open && active && visible && selected.key === key;
    pending = controller;
    notice = "";
    retryOffset = undefined;
    try {
      const result = await read(selected, offset, limit, controller.signal);
      if (!current()) return;
      switch (result.state) {
        case "ready":
          page = result;
          if (region) region.scrollTop = 0;
          break;
        case "capacity":
          if (result.kind === "readers_busy") {
            notice = "Diff readers are busy.";
            retryOffset = offset;
          } else notice = "Diff read limit reached.";
          break;
        case "source_changed": notice = "Conversation changed. Refresh to read this diff."; break;
        case "invalid_range": notice = "This diff page is unavailable."; break;
        case "unavailable": notice = "Diff unavailable."; break;
      }
    } catch {
      if (!current()) return;
      notice = "Could not read diff.";
      retryOffset = offset;
    } finally {
      if (pending === controller) pending = undefined;
    }
  }

  function navigate(offset: number): void {
    if (pending || !active || !visible) return;
    if (header) anchor(header);
    void load(offset);
  }
</script>

<details class="work-diff" bind:open>
  <summary bind:this={header} onclick={(event) => anchor(event.currentTarget)}>
    <span class="diff-marker" aria-hidden="true">±</span>
    <span class="diff-title" title={target.title}>{target.title || "Diff"}</span>
  </summary>
  {#if open && visible}
    <div class="diff-bar">
      <button type="button" aria-label="Previous diff page" title="Previous diff page"
        aria-disabled={!!pending || !active || !page?.offset}
        onclick={() => { if (page?.offset) navigate(Math.max(0, page.offset - limit)); }}>‹</button>
      <span role="status">
        {#if pending}Reading diff…
        {:else if notice}{notice}
        {:else if page}{page.offset + 1}–{page.next_offset} / {page.total_lines}
        {/if}
      </span>
      {#if retryOffset !== undefined}
        <button type="button" onclick={() => { if (retryOffset !== undefined) navigate(retryOffset); }}>Retry</button>
      {/if}
      <button type="button" aria-label="Next diff page" title="Next diff page"
        aria-disabled={!!pending || !active || !page || page.next_offset >= page.total_lines}
        onclick={() => { if (page && page.next_offset < page.total_lines) navigate(page.next_offset); }}>›</button>
    </div>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (The bounded diff region supports keyboard scrolling.) -->
    <div class="diff-lines" bind:this={region} role="region" aria-label="Unified diff" aria-busy={!!pending} tabindex="0">
      {#each page?.lines ?? [] as line}
        <div class="diff-line" data-kind={line.kind}>
          <span class="number" title="Before">{line.before ?? ""}</span>
          <span class="number" title="After">{line.after ?? ""}</span>
          <code>{line.text}{line.truncated ? "…" : ""}</code>
        </div>
      {/each}
    </div>
  {/if}
</details>

<style>
  .work-diff { border-inline-start: 1px dashed var(--line); padding: 0.2rem 0.75rem; min-inline-size: 0; }
  summary { cursor: pointer; padding: 0.2rem 0.35rem; font: 0.83rem/1.45 var(--atman-mono); color: var(--muted); }
  summary:hover { background: var(--hover); color: var(--fg); }
  .diff-marker { color: var(--accent); margin-inline-end: 0.35rem; }
  .diff-title { overflow-wrap: anywhere; }
  .diff-bar { display: flex; align-items: center; gap: 0.45rem; color: var(--muted); font: 0.75rem/1.4 var(--atman-mono); }
  .diff-bar span { flex: 1; }
  .diff-bar button { padding: 0.2rem 0.55rem; min-block-size: 1.65rem; }
  button[aria-disabled="true"] { opacity: 0.5; cursor: default; }
  .diff-lines {
    block-size: 14rem; overflow: auto; overscroll-behavior: contain;
    margin-block: 0.25rem 0.4rem; border: 1px solid var(--line); background: var(--surface);
  }
  .diff-lines:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
  .diff-line {
    display: grid; grid-template-columns: 5ch 5ch minmax(0, 1fr);
    padding-inline: 0.4rem; font: 0.8rem/1.55 var(--atman-mono);
  }
  .number { color: var(--muted); text-align: right; padding-inline-end: 0.45rem; overflow: hidden; }
  code { font: inherit; white-space: pre-wrap; overflow-wrap: anywhere; }
  [data-kind="added"] { background: color-mix(in srgb, var(--accent) 13%, var(--surface)); }
  [data-kind="removed"] { background: color-mix(in srgb, var(--danger) 16%, var(--surface)); }
  [data-kind="hunk"] { color: var(--muted); background: var(--hover); }
  [data-kind="header"] { color: var(--fg); font-weight: 600; border-block-start: 1px solid var(--line); }
</style>
