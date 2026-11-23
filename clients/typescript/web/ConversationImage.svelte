<script lang="ts">
  import { untrack } from "svelte";
  import { imageBlob, type ImageTarget, type ReadImage } from "./conversation-image.ts";

  let { target, read, active, visible, anchor }: {
    target: ImageTarget | undefined;
    read: ReadImage | undefined;
    active: boolean;
    visible: boolean;
    anchor: (node: HTMLElement) => void;
  } = $props();

  let open = $state(false);
  let url = $state<string>();
  let pending = $state(false);
  let notice = $state<{ text: string; retry: boolean }>();
  let attempts = $state(0);
  const key = $derived(target?.key);
  const available = $derived(read !== undefined);

  $effect(() => {
    if (!open || !active || !visible || !available || key === undefined) return;
    const observedKey = key;
    const attempt = attempts;
    const observation = new AbortController();
    const isCurrent = () => !observation.signal.aborted && open && active && visible &&
      attempts === attempt && key === observedKey;
    const selected = untrack(() => ({ target: target!, read: read! }));
    let owned: string | undefined;
    pending = true;
    void selected.read(selected.target, observation.signal).then(result => {
      if (!isCurrent()) return;
      switch (result.state) {
        case "ready":
          owned = URL.createObjectURL(imageBlob(result));
          url = owned;
          break;
        case "byte_limit":
          notice = { text: `Image exceeds the ${result.allowed / (1024 * 1024)} MiB read limit.`, retry: false };
          break;
        case "source_changed":
          notice = { text: "Conversation changed. Refresh to read this image.", retry: false };
          break;
        case "content_changed":
          notice = { text: "Stored image changed.", retry: true };
          break;
        case "capacity":
          notice = result.kind === "readers_busy"
            ? { text: "Image readers are busy.", retry: true }
            : { text: "Conversation read limit reached.", retry: false };
          break;
        case "unavailable":
          notice = { text: "Image unavailable.", retry: true };
          break;
      }
    }).catch(() => {
      if (isCurrent()) notice = { text: "Could not read image.", retry: true };
    }).finally(() => {
      if (isCurrent()) pending = false;
    });
    return () => {
      observation.abort();
      if (owned) URL.revokeObjectURL(owned);
      url = undefined;
      pending = false;
      notice = undefined;
    };
  });

  function decodeError(node: Element): void {
    if (url === undefined || node.getAttribute("src") !== url) return;
    URL.revokeObjectURL(url);
    url = undefined;
    notice = { text: "Image could not be displayed.", retry: true };
  }
</script>

{#if available && target}
  <details class="conversation-image" bind:open>
    <summary onclick={(event) => anchor(event.currentTarget)}>Image</summary>
    {#if open && visible}
      <div class="image-frame" aria-busy={pending}>
        {#if url}
          <img src={url} alt="Conversation attachment" decoding="async" onerror={(event) => decodeError(event.currentTarget)} />
          <a href={url} target="_blank" rel="noopener noreferrer" aria-label="Open original image" title="Open original image">↗</a>
        {:else if notice}
          <div class="image-status">
            <span role="status">{notice.text}</span>
            {#if notice.retry}<button type="button" onclick={() => attempts += 1}>Retry</button>{/if}
          </div>
        {:else if pending}
          <span role="status">Reading image…</span>
        {/if}
      </div>
    {/if}
  </details>
{:else}
  <p class="entry-muted">Image attachment</p>
{/if}

<style>
  .entry-muted, summary, .image-frame { color: var(--muted); }
  .conversation-image { padding: 0.2rem 0.75rem; border-inline-start: 1px dashed var(--line); }
  summary { cursor: pointer; padding: 0.2rem 0.35rem; font: 0.83rem/1.45 var(--atman-mono); }
  summary:hover { background: var(--hover); }
  .image-frame {
    position: relative;
    display: grid;
    place-items: center;
    block-size: clamp(8rem, 30dvh, 14rem);
    min-inline-size: 0;
    margin-block: 0.2rem 0.4rem;
    padding: 0.55rem 0.65rem;
    border: 1px solid var(--line);
    background: var(--surface);
    font-size: 0.83rem;
  }
  img { inline-size: 100%; block-size: 100%; min-block-size: 0; object-fit: contain; }
  a {
    position: absolute;
    inset-block-start: 0.35rem;
    inset-inline-end: 0.35rem;
    padding: 0.2rem 0.5rem;
    border: 1px solid var(--line);
    background: var(--surface);
    color: var(--fg);
    text-decoration: none;
  }
  a:hover { background: var(--hover); }
  .image-status { display: flex; flex-wrap: wrap; align-items: center; justify-content: center; gap: 0.4rem; }
  .image-status button { padding: 0.2rem 0.55rem; }
</style>
