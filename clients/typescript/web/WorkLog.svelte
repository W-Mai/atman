<script lang="ts">
  import { onDestroy, tick, untrack } from "svelte";
  import type { Dtos } from "../src/index.ts";
  import { LOG_PAGE_BYTES, sameCheckpoint, type LogProgress, type LogTarget, type ReadLog } from "./work-log.ts";

  type Page = Extract<Dtos["ReadConversationLogResponse"], { state: "ready" }>;
  type Failure = Exclude<Dtos["ReadConversationLogResponse"], Page | LogProgress>;

  let { target, read, visible, anchor }: {
    target: LogTarget;
    read: ReadLog;
    visible: boolean;
    anchor?: (node: HTMLElement) => void;
  } = $props();

  let open = $state(false);
  let details: HTMLDetailsElement;
  let region = $state<HTMLDivElement>();
  let checkpoint = $state.raw<LogTarget>();
  let page = $state.raw<Page>();
  let progress = $state.raw<LogProgress>();
  let busy = $state(false);
  let observing = $state(false);
  let error = $state("");
  let pending: AbortController | undefined;
  let scrollTop = 0;
  let failed: { source: LogTarget; index: number; offset: number } | undefined;
  let stops = $state.raw<number[]>([]);
  let position = $state(0);
  const newer = $derived(checkpoint !== undefined && !sameCheckpoint(checkpoint, target));
  const reload = $derived(checkpoint !== undefined && checkpoint.bytes == null && !observing && !newer);
  const canPage = $derived(!!checkpoint && !reload && (observing || checkpoint.bytes != null));
  const size = $derived(target.bytes ?? page?.total_bytes);
  const status = $derived(target.drained === undefined ? "Historical log" :
    target.drained ? "Writer finished" : "Writer active");
  const numbers = new Intl.NumberFormat();

  function retire(): void {
    pending?.abort();
    pending = undefined;
    busy = false;
    observing = false;
    progress = undefined;
  }

  $effect(() => {
    const shown = visible && open;
    untrack(() => {
      if (!shown) {
        retire();
      } else {
        if (!page && !error) void load(target, 0, 0);
        void tick().then(() => { if (region) region.scrollTop = scrollTop; });
      }
    });
  });
  onDestroy(retire);

  function failure(value: Failure): string {
    switch (value.state) {
      case "capacity": return value.kind === "readers_busy" ? "Log readers are busy." : "Conversation read limit reached.";
      case "verification_budget": return `Log exceeds the ${numbers.format(value.allowed / 1048576)} MiB read limit.`;
      case "source_changed": return "Conversation changed.";
      case "content_changed": return "Stored log changed.";
      case "invalid_range": return "Log position unavailable.";
      case "verification_cancelled": return "Reading paused.";
      case "unavailable": return "Log unavailable.";
    }
  }

  async function load(source: LogTarget, index: number, offset: number): Promise<void> {
    if (!visible || !open || busy) return;
    const controller = pending ??= new AbortController();
    observing = true;
    const current = () => pending === controller && !controller.signal.aborted && visible && open;
    error = "";
    busy = true;
    failed = { source, index, offset };
    try {
      const result = await read(source, offset, controller.signal, value => {
        if (current()) progress = value;
      });
      if (!current()) return;
      if (result.state !== "ready" || (source.bytes != null && result.total_bytes !== source.bytes)) {
        error = result.state === "ready" ? "Log size differs from its checkpoint." : failure(result);
        if (source.bytes == null) failed = { source, index: 0, offset: 0 };
        retire();
        return;
      }
      if (stops[index] !== offset) stops = [...stops.slice(0, index), offset];
      position = index;
      checkpoint = source;
      page = result;
      failed = undefined;
      scrollTop = 0;
      if (region) region.scrollTop = 0;
    } catch {
      if (current()) error = "Could not read log.";
    } finally {
      if (pending === controller) {
        progress = undefined;
        busy = false;
      }
    }
  }

  function latest(): void {
    const displayed = page;
    const advance = displayed && target.bytes != null && !displayed.has_more && target.bytes - displayed.offset > LOG_PAGE_BYTES &&
      target.bytes > displayed.total_bytes && !displayed.content.endsWith("�");
    const reset = reload || checkpoint?.bytes == null;
    const offset = reset ? 0 : advance ? displayed.next_offset : displayed?.offset ?? 0;
    const index = reset ? 0 : advance ? position + 1 : position;
    retire();
    if (reset) stops = [];
    void load(target, index, offset);
  }

  function toggle(): void {
    open = details.open;
    if (!open) retire();
  }
</script>

<details bind:this={details} class="work-log" ontoggle={toggle}>
  <summary onclick={() => anchor?.(details)}>
    <span class="marker" class:drained={target.drained !== false} aria-label={status} title={status}>●</span>
    <span>Bash log</span>
    {#if size != null}<span class="size" title={target.bytes != null ? "Latest checkpoint size" : "Observed log size"}>{numbers.format(size)} B</span>{/if}
  </summary>
  {#if open && visible}
    <div class="log-bar" role="group" aria-label="Bash log controls">
      <button type="button" disabled={busy || position === 0 || !canPage} aria-label="Previous log page"
        title="Previous page" onclick={() => checkpoint && load(checkpoint, position - 1, stops[position - 1]!)}>←</button>
      <button type="button" disabled={busy || !page?.has_more || !canPage} aria-label="Next log page"
        title="Next page" onclick={() => checkpoint && page && load(checkpoint, position + 1, page.next_offset)}>→</button>
      <span class="range">{page ? `${numbers.format(page.offset)}–${numbers.format(page.next_offset)} / ${numbers.format(page.total_bytes)} B` : ""}</span>
      <button type="button" disabled={busy || (!newer && !reload)} onclick={latest}>{reload ? "Reload" : "Latest"}</button>
    </div>
    <div class="log-progress">
      {#if progress}
        <progress aria-label="Verifying Bash log" value={progress.verified_bytes} max={progress.total_bytes}></progress>
      {/if}
    </div>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (Allow independent keyboard scrolling of the log.) -->
    <div bind:this={region} class="log-body" role="region" aria-label="Bash log output" tabindex="0" aria-busy={busy}
      onscroll={(event) => { scrollTop = event.currentTarget.scrollTop; }}>
      {#if error}
        <div class="log-message" role="status">{error}
          <button type="button" onclick={() => failed && load(failed.source, failed.index, failed.offset)}>Retry</button>
        </div>
      {:else if page?.content}
        <pre>{page.content}</pre>
      {:else}
        <p class="log-message" role="status">{busy ? "Reading log…" : "No output yet."}</p>
      {/if}
    </div>
  {/if}
</details>

<style>
  .work-log { min-width: 0; color: var(--fg); }
  summary, .log-bar { display: flex; align-items: center; gap: .45rem; min-height: 2rem; padding-inline: .55rem; }
  summary { cursor: pointer; list-style: none; }
  summary::-webkit-details-marker { display: none; }
  summary:hover, summary:focus-visible { background: var(--surface); }
  .marker { color: var(--accent); font-size: .7em; }
  .marker.drained, .range, .size, .log-message { color: var(--muted); }
  .size { margin-inline-start: auto; white-space: nowrap; }
  .size, .range { font-variant-numeric: tabular-nums; font-size: .8em; }
  .range { flex: 1; min-width: 0; overflow-wrap: anywhere; }
  .log-bar { border-block: 1px solid var(--line); flex-wrap: wrap; }
  .log-bar button, .log-message button { min-height: 1.6rem; padding: .12rem .42rem; font-size: .85em; }
  .log-body { block-size: clamp(6rem, 23vh, 13rem); min-width: 0; overflow: auto; overscroll-behavior: contain; }
  pre, .log-message { padding: .55rem; margin: 0; }
  pre { font: inherit; font-size: .85em; white-space: pre-wrap; overflow-wrap: anywhere; }
  .log-progress { block-size: 2px; }
  progress { display: block; inline-size: 100%; block-size: 2px; accent-color: var(--accent); }
</style>
