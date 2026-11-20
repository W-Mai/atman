<script lang="ts">
  import type { Dtos } from "../src/index.ts";
  import ComposerSettings from "./ComposerSettings.svelte";
  import { clipboardImages, imageMimeTypes, readComposerImages } from "./composer-images.ts";
  import { projectReasoning } from "./composer-settings.ts";
  import type { DraftAttachments } from "./tab-drafts.ts";

  type View = {
    text: string;
    feedback: string;
    canSend: boolean;
    canCheck: boolean;
    canRetry: boolean;
    editable: boolean;
    hasPending: boolean;
    hasEarlierView: boolean;
    canShowHistory?: boolean;
    attachments?: DraftAttachments;
    attachmentsMemoryOnly?: boolean;
    settings?: Dtos["FlowComposerResponse"];
  };

  let { edit, send, checkDelivery, retry, jumpLatest, showHistory, addImages, removeAttachment, browseModels }: {
    edit: (value: string) => void;
    send: () => void;
    checkDelivery: () => void;
    retry: () => void;
    jumpLatest: () => void;
    showHistory: (trigger: HTMLButtonElement) => void;
    addImages: (identity: number, images: readonly Dtos["InlineImage"][]) => string | undefined;
    removeAttachment: (attachment: Dtos["InlineImage"] | "quote") => void;
    browseModels: (query: string, signal: AbortSignal) => Promise<Dtos["FlowComposerResponse"] | undefined>;
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
  let filePicker = $state<HTMLInputElement>();
  let readingImages = $state(false);
  let imageFeedback = $state("");
  let composing = false;
  const cells = Array.from({ length: 8 }, (_, index) => index);
  let model = $derived(view.settings?.model);
  let trust = $derived(view.settings?.trust);
  let escalation = $derived(trust?.mode === "eager" ? trust.escalation : undefined);
  let meter = $derived(model?.status === "ready" ? projectReasoning(model.reasoning) : undefined);

  let binding = $state(0);
  let settingsOpen = $state(false);
  let browseTrigger: HTMLButtonElement | undefined;

  function openSettings(event: MouseEvent & { currentTarget: HTMLButtonElement }): void {
    if (!current(event)) return;
    browseTrigger = event.currentTarget;
    settingsOpen = true;
  }

  function closeSettings(): void {
    settingsOpen = false;
    const trigger = browseTrigger;
    browseTrigger = undefined;
    if (trigger?.isConnected) trigger.focus();
  }

  export function update(next: View, identity: number): void {
    if (binding !== identity) {
      composing = false;
      readingImages = false;
      imageFeedback = "";
      settingsOpen = false;
      browseTrigger = undefined;
    }
    binding = identity;
    view = next;
  }

  function current(event: Event): boolean {
    return (event.currentTarget as HTMLElement).dataset.binding === String(binding);
  }

  function onKeydown(event: KeyboardEvent): void {
    if (!current(event) || event.key !== "Enter" || event.shiftKey || event.isComposing || composing || event.keyCode === 229) return;
    event.preventDefault();
    if (!event.repeat && view.canSend && !readingImages) send();
  }

  function insertText(text: string): void {
    if (!editor || !view.editable) return;
    editor.setRangeText(text, editor.selectionStart, editor.selectionEnd, "end");
    edit(editor.value);
    editor.focus();
  }

  function insertNewline(event: Event): void {
    if (current(event)) insertText("\n");
  }

  async function attach(files: File[]): Promise<void> {
    if (!files.length || !view.editable) return;
    if (readingImages || view.hasPending) {
      imageFeedback = readingImages ? "Wait for the current image selection." : "Resolve delivery before adding images.";
      return;
    }
    const identity = binding;
    readingImages = true;
    imageFeedback = "";
    const mounted = () => binding === identity && editor?.isConnected === true;
    const stillCurrent = () => mounted() && view.editable && !view.hasPending;
    try {
      const images = await readComposerImages(files, view.attachments?.images ?? [], stillCurrent);
      if (stillCurrent()) {
        const issue = addImages(identity, images);
        if (issue) imageFeedback = issue;
      }
    } catch (error) {
      if (mounted())
        imageFeedback = error instanceof Error ? error.message : "Could not read images.";
    } finally {
      if (mounted()) readingImages = false;
    }
  }

  function paste(event: ClipboardEvent): void {
    if (!current(event) || !view.editable || !event.clipboardData) return;
    const files = clipboardImages(event.clipboardData);
    if (!files.length) return;
    event.preventDefault();
    const text = event.clipboardData.getData("text/plain");
    if (text) insertText(text.replace(/\r\n?/g, "\n"));
    void attach(files);
  }

  function chooseFiles(event: Event & { currentTarget: HTMLInputElement }): void {
    if (!current(event)) return;
    const files = Array.from(event.currentTarget.files ?? []);
    event.currentTarget.value = "";
    void attach(files);
  }

  function remove(event: Event, attachment: Dtos["InlineImage"] | "quote"): void {
    if (!current(event)) return;
    imageFeedback = "";
    removeAttachment(attachment);
    editor?.focus();
  }
</script>

{#key binding}
{#if view.attachments || readingImages}
  <div class="attachment-strip" role="group" aria-label="Attachments">
    {#if view.attachments?.quote !== undefined}
      <button type="button" class="console-button attachment" aria-label="Remove quote" title="Remove quote"
        disabled={!view.editable || view.hasPending} data-binding={binding} onclick={(event) => remove(event, "quote")}>
        <span>Quote</span><span aria-hidden="true">×</span>
      </button>
    {/if}
    {#each view.attachments?.images ?? [] as image (image)}
      <button type="button" class="console-button attachment" aria-label={"Remove " + (image.name ?? "image")}
        title={"Remove " + (image.name ?? "image")} disabled={!view.editable || view.hasPending} data-binding={binding}
        onclick={(event) => remove(event, image)}><span>{image.name ?? "Image"}</span><span aria-hidden="true">×</span></button>
    {/each}
    {#if readingImages}<span class="attachment-state" role="status">Reading images…</span>{/if}
    {#if view.attachmentsMemoryOnly}<span class="attachment-state" role="status">Draft with attachments is only available in this tab</span>{/if}
  </div>
{/if}
<form class="input-console" data-binding={binding} onsubmit={(event) => { event.preventDefault(); if (current(event) && view.canSend && !readingImages) send(); }}>
  <div class="console-top">
    <div class="input-heading">
      <span class="prompt" aria-hidden="true">&gt;</span>
      {#if view.hasEarlierView}
        <button type="button" class="console-button latest" data-binding={binding} onclick={(event) => { if (current(event)) jumpLatest(); }}
          title="Scroll to latest">↓ Latest</button>
      {/if}
    </div>
    <div class="top-controls">
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
      {#if view.canShowHistory}
        <button type="button" class="console-button" aria-label="Conversation branches" title="Conversation branches"
          data-binding={binding} onclick={(event) => { if (current(event)) showHistory(event.currentTarget); }}>⑂</button>
      {/if}
      <button type="button" class="console-button" aria-label="Attach images" title="Attach images"
        disabled={!view.editable || view.hasPending || readingImages} data-binding={binding}
        onclick={(event) => { if (current(event)) filePicker?.click(); }}>+</button>
    </div>
  </div>

  <input type="file" hidden multiple accept={imageMimeTypes} bind:this={filePicker} data-binding={binding} onchange={chooseFiles} />
  <textarea bind:this={editor} data-binding={binding} rows="4" aria-label="Message" placeholder="Message" value={view.text}
    readonly={!view.editable}
    oninput={(event) => { if (current(event)) edit(event.currentTarget.value); }}
    oncompositionstart={(event) => { if (current(event)) composing = true; }}
    oncompositionend={(event) => { if (current(event)) composing = false; }}
    onkeydown={onKeydown} onpaste={paste}></textarea>

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
      {#if view.settings}
        <button type="button" class="model" title="Browse Models" aria-label="Browse Models" aria-haspopup="dialog"
          data-binding={binding} onclick={openSettings}>
          {model?.status === "ready" ? model.model : model?.status === "pending" ? "Models" : "Model unavailable"}
        </button>
      {/if}
    </div>
    <div class="composer-buttons">
      <button type="button" class="console-button" aria-label="Insert newline" disabled={!view.editable}
        title="Insert newline" data-binding={binding} onclick={insertNewline}>↵</button>
      <button type="submit" class="console-button send" aria-label="Send message" title="Send message"
        disabled={!view.canSend || readingImages}>↑</button>
    </div>
  </div>
</form>
{#if settingsOpen && view.settings}
  <ComposerSettings settings={view.settings} browse={browseModels} close={closeSettings} />
{/if}
{#if view.hasPending || view.feedback || imageFeedback}
  <div class="delivery">
    <span role="status">{view.hasPending ? view.feedback : imageFeedback || view.feedback}</span>
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
  .attachment-strip {
    display: flex;
    flex-wrap: wrap;
    gap: .25rem .5rem;
    align-items: center;
    margin-block-end: .45rem;
    color: var(--muted);
    font-size: .78rem;
  }

  .attachment {
    display: inline-flex;
    align-items: center;
    gap: .5rem;
    min-inline-size: 0;
    max-inline-size: min(20ch, 100%);
    font: inherit;
  }

  .attachment span:first-child {
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .attachment span:last-child { color: var(--accent); }
  .attachment-state { padding-inline: .45rem; }
  .attachment-strip .attachment { border: 1px solid var(--line); }
  .input-console,
  .attachment-strip,
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
  .top-controls,
  .trust,
  .model-settings,
  .composer-buttons {
    display: flex;
    align-items: center;
    gap: 0.45rem;
    min-inline-size: 0;
  }

  .input-heading,
  .top-controls,
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
    --editor-padding: 0.25rem;
    display: block;
    inline-size: 100%;
    field-sizing: content;
    min-block-size: min(36dvh, calc(4lh + 2 * var(--editor-padding)));
    max-block-size: min(36dvh, calc(12lh + 2 * var(--editor-padding)));
    padding: var(--editor-padding) 0.15rem;
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

  .model {
    min-inline-size: 0;
    border: 0;
    border-radius: 0;
    color: inherit;
    background: transparent;
    padding-inline: .45rem;
    font: inherit;
  }

  .model:hover,
  .model:focus-visible {
    color: var(--fg);
    background: var(--hover);
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
