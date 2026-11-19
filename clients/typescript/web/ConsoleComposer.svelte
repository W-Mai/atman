<script lang="ts">
  import type { Dtos } from "../src/index.ts";
  import { projectReasoning } from "./composer-settings.ts";

  type View = {
    text: string;
    feedback: string;
    canSend: boolean;
    canCheck: boolean;
    canRetry: boolean;
    editable: boolean;
    hasPending: boolean;
    hasEarlierView: boolean;
    settings?: Dtos["FlowComposerResponse"];
  };

  let { edit, send, checkDelivery, retry, jumpLatest }: {
    edit: (value: string) => void;
    send: () => void;
    checkDelivery: () => void;
    retry: () => void;
    jumpLatest: () => void;
  } = $props();

  let view: View = $state.raw({
    text: "",
    feedback: "",
    canSend: false,
    canCheck: false,
    canRetry: false,
    editable: false,
    hasPending: false,
    hasEarlierView: false,
  });
  let editor = $state<HTMLTextAreaElement>();
  let composing = false;
  const cells = Array.from({ length: 8 }, (_, index) => index);
  let model = $derived(view.settings?.model);
  let trust = $derived(view.settings?.trust);
  let escalation = $derived(trust?.mode === "eager" ? trust.escalation : undefined);
  let meter = $derived(model?.status === "ready" ? projectReasoning(model.reasoning) : undefined);

  let binding = $state(0);

  export function update(next: View, identity: number): void {
    if (binding !== identity) composing = false;
    binding = identity;
    view = next;
  }

  function current(event: Event): boolean {
    return (event.currentTarget as HTMLElement).dataset.binding === String(binding);
  }

  function onKeydown(event: KeyboardEvent): void {
    if (!current(event) || event.key !== "Enter" || event.shiftKey || event.isComposing || composing || event.keyCode === 229) return;
    event.preventDefault();
    if (!event.repeat && view.canSend) send();
  }

  function insertNewline(event: Event): void {
    if (!editor || !current(event)) return;
    const { selectionStart, selectionEnd } = editor;
    editor.setRangeText("\n", selectionStart, selectionEnd, "end");
    edit(editor.value);
    editor.focus();
  }
</script>

{#key binding}
<form class="input-console" data-binding={binding} onsubmit={(event) => { event.preventDefault(); if (current(event) && view.canSend) send(); }}>
  <div class="console-top">
    <div class="input-heading">
      <span class="prompt" aria-hidden="true">&gt;</span>
      {#if view.hasEarlierView}
        <button type="button" class="console-button latest" data-binding={binding} onclick={(event) => { if (current(event)) jumpLatest(); }}
          title="Scroll to latest">↓ Latest</button>
      {/if}
    </div>
    {#if trust}
      <div class="trust" role="group" aria-label="Permissions">
        <span class="trust-mode" data-color={trust.color} title={trust.description}>{trust.icon} {trust.name}</span>
        {#if escalation}
          <span class="divider" aria-hidden="true">╱</span>
          <span class="trust-mode" data-color={escalation.color}
            title={"Eager escalation · " + escalation.name}>{escalation.icon} {escalation.name}</span>
        {/if}
      </div>
    {/if}
  </div>

  <textarea bind:this={editor} data-binding={binding} rows="4" aria-label="Message" placeholder="Message" value={view.text}
    readonly={!view.editable}
    oninput={(event) => { if (current(event)) edit(event.currentTarget.value); }}
    oncompositionstart={(event) => { if (current(event)) composing = true; }}
    oncompositionend={(event) => { if (current(event)) composing = false; }}
    onkeydown={onKeydown}></textarea>

  <div class="console-bottom">
    <div class="model-settings" role="group" aria-label="Model and reasoning">
      {#if meter}
        <span class="reasoning" title={meter.title} data-peak={meter.filled === cells.length}>
          <span class="sr-only">{meter.title}</span>
          <span class="meter" aria-hidden="true">
            {#each cells as cell (cell)}
              <i class:filled={cell < meter.filled}></i>
            {/each}
          </span>
        </span>
      {/if}
      {#if model?.status === "ready"}
        <span class="model" title={model.model + " · " + model.provider}>{model.model}</span>
      {:else if model?.status === "unavailable"}
        <span class="model" title={model.reason.replaceAll("_", " ")}>Model unavailable</span>
      {/if}
    </div>
    <div class="composer-buttons">
      <button type="button" class="console-button" aria-label="Insert newline" disabled={!view.editable}
        title="Insert newline" data-binding={binding} onclick={insertNewline}>↵</button>
      <button type="submit" class="console-button send" aria-label="Send message" title="Send message"
        disabled={!view.canSend}>↑</button>
    </div>
  </div>
</form>
{#if view.hasPending || view.feedback}
  <div class="delivery">
    <span role="status">{view.feedback}</span>
    {#if view.hasPending}
      <button type="button" class="console-button" disabled={!view.canCheck}
        data-binding={binding} onclick={(event) => { if (current(event)) checkDelivery(); }}>Check delivery</button>
      {#if view.canRetry}
        <button type="button" class="console-button" data-binding={binding} onclick={(event) => { if (current(event)) retry(); }}>Retry original send</button>
      {/if}
    {/if}
  </div>
{/if}

{/key}

<style>
  .input-console,
  .delivery {
    max-inline-size: 80ch;
    margin-inline: auto;
    font-family: var(--atman-mono);
  }

  .input-console {
    position: relative;
    border: 1px solid var(--line);
    background: var(--bg);
    padding: 0.55rem 0.75rem 0.65rem;
  }

  .input-console:focus-within {
    border-color: var(--accent);
  }

  .console-top,
  .console-bottom {
    position: absolute;
    inset-inline: 0.65rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.35rem;
    min-inline-size: 0;
    line-height: 1;
  }

  .console-top {
    inset-block-start: 0;
    translate: 0 -50%;
  }

  .console-bottom {
    inset-block-end: 0;
    translate: 0 50%;
  }

  .input-heading,
  .trust,
  .model-settings,
  .composer-buttons {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    min-inline-size: 0;
  }

  .input-heading,
  .trust,
  .model-settings:not(:empty),
  .composer-buttons {
    padding-inline: 0.25rem;
    background: var(--bg);
  }

  .composer-buttons {
    flex: none;
  }

  .prompt,
  .send:not(:disabled) {
    color: var(--accent);
  }

  .prompt {
    padding-inline: 0.15rem;
    font-size: 1rem;
    font-weight: bold;
  }

  textarea {
    display: block;
    width: 100%;
    max-block-size: 28dvh;
    box-sizing: border-box;
    padding: 0.25rem 0.15rem;
    border: 0;
    resize: none;
    color: var(--fg);
    background: transparent;
    font: 0.92rem/1.55 var(--atman-mono);
  }

  textarea:focus-visible {
    outline: 0;
  }

  .console-button {
    min-block-size: 1.8rem;
    padding: 0.18rem 0.45rem;
    border: 0;
    border-radius: 0;
    color: var(--muted);
    background: var(--bg);
    white-space: nowrap;
  }

  .console-button:not(:disabled):hover {
    color: var(--fg);
    background: var(--hover);
  }

  .console-button.send:not(:disabled):hover {
    color: var(--accent);
  }

  .latest,
  .trust,
  .model-settings,
  .delivery {
    font-size: 0.78rem;
  }

  .latest {
    color: var(--accent);
  }

  .trust-mode,
  .model {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    line-height: 1.2;
    padding-block: 0.2rem;
  }

  .trust-mode {
    color: var(--muted);
  }

  .trust-mode[data-color="cyan"] { color: var(--accent); }
  .trust-mode[data-color="green"] { color: var(--success, var(--fg)); }
  .trust-mode[data-color="yellow"],
  .trust-mode[data-color="orange"] { color: var(--warn, var(--fg)); }
  .trust-mode[data-color="red"] { color: var(--danger); }
  .divider { color: var(--faint); }

  .model-settings {
    max-inline-size: calc(100% - 4.8rem);
    color: var(--muted);
  }

  .reasoning {
    display: inline-flex;
    align-items: center;
    padding: 0.28rem 0.1rem;
  }

  .meter {
    flex: none;
    display: grid;
    grid-template-columns: repeat(8, 0.45ch);
    gap: 0.1ch;
  }

  .meter i {
    height: 2px;
    background: var(--faint);
  }

  .meter .filled {
    background: var(--accent);
  }

  .reasoning[data-peak="true"] .meter {
    background: var(--reasoning-peak, linear-gradient(90deg, var(--danger), var(--warn, var(--accent)), var(--accent), var(--success, var(--accent))));
  }

  .reasoning[data-peak="true"] .meter .filled {
    background: transparent;
  }

  .delivery {
    display: flex;
    align-items: center;
    gap: 0.5rem 0.75rem;
    flex-wrap: wrap;
    margin-block-start: 0.95rem;
    color: var(--muted);
  }

  .delivery [role="status"] {
    flex: 1;
    overflow-wrap: anywhere;
  }

  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    clip-path: inset(50%);
    overflow: hidden;
    white-space: nowrap;
  }

  @media (max-width: 430px) {
    .console-button:not(.latest) {
      min-inline-size: 2rem;
      min-block-size: 2rem;
    }
  }
</style>
