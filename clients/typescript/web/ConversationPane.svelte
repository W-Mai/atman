<script lang="ts">
  import type { Dtos } from "../src/index.ts";

  type Item = Dtos["ConversationItem"];
  type View = {
    source: string;
    items: readonly Item[];
    placeholder: string;
    hasEarlier: boolean;
    loading: boolean;
  };

  let { loadEarlier, refresh }: {
    loadEarlier: () => void;
    refresh: () => void;
  } = $props();

  let view: View = $state.raw({
    source: "",
    items: [],
    placeholder: "Loading conversation",
    hasEarlier: false,
    loading: true,
  });

  export function update(next: View): void {
    view = next;
  }

  function recordKey({ commit, fact, item, message }: Dtos["ConversationItemId"]): string {
    return view.source + ":" + [commit, fact, item, message].join(":");
  }
</script>

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

  {#if view.items.length === 0}
    <p class="transcript-placeholder">
      {#if view.loading}<span class="loading-mark" aria-hidden="true"></span>{/if}
      {view.placeholder}
    </p>
  {:else}
    <div class="transcript-items">
      {#each view.items as item (recordKey(item.id))}
        {@const body = item.body}
        {#if body.kind === "deferred"}
          <article data-conversation-item class="entry entry-muted">Message exceeds the browser page limit.</article>
        {:else if body.parts.length > 0}
          <article data-conversation-item class="entry" class:authored={body.role === "user"}
            class:entry-muted={body.role === "system" || body.role === "tool"}>
            {#each body.parts as part}
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
            {/each}
          </article>
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

  .authored {
    padding-block: 0.75rem;
    background: var(--user-bg);
  }

  .entry-muted {
    color: var(--muted);
    font-size: 0.83rem;
  }

  blockquote {
    margin: 0.35rem 0;
    padding-inline-start: 0.75rem;
    border-inline-start: 1px dashed var(--line);
    color: var(--muted);
  }

  .thinking {
    padding: 0.2rem 0.75rem;
    border-inline-start: 1px dashed var(--line);
    color: var(--muted);
  }

  .thinking summary {
    list-style: none;
    cursor: pointer;
    padding: 0.2rem 0.35rem;
    font: 0.83rem/1.45 var(--atman-mono);
  }

  .thinking summary::-webkit-details-marker {
    display: none;
  }

  .thinking summary:hover {
    background: var(--hover);
  }

  .thinking summary span {
    margin-inline-end: 0.25rem;
    color: var(--success, var(--accent));
  }

  .thinking p {
    margin-block: 0.35rem 0.55rem;
  }
</style>
