<script lang="ts">
  import { tick } from "svelte";
  import type { Dtos } from "../src/index.ts";
  import type { InteractionKind, InteractionView } from "./interaction-activity.ts";
  import FlowFormReply from "./FlowFormReply.svelte";

  let { view, refresh, check, dismissUnknown, issue }: {
    view: InteractionView;
    refresh: () => void;
    check: (view: InteractionView) => void;
    dismissUnknown: (view: InteractionView) => void;
    issue: (view: InteractionView, id: string, kind: InteractionKind, reply: Dtos["FlowInteractionReply"]) => void;
  } = $props();

  const receipts: Record<NonNullable<InteractionView["notice"]>, string> = {
    pending: "Waiting for the owner…", unknown: "Reply unconfirmed.", not_sent: "Reply wasn’t sent.",
    resumed: "Reply delivered to the waiting Flow.", deferred: "Answer accepted into the Flow’s inbox.",
    dismissed: "Form dismissed.", decided: "Decision delivered. The Flow applies any context change separately.",
    invalid_answer: "The owner rejected this answer. Review the choices and try again.",
    unavailable: "This request has expired or was handled elsewhere.",
  };
  let ready = $derived(view.snapshot?.state === "ready" ? view.snapshot : undefined);
  let compact = $derived(ready?.compact ?? undefined);
  let edit = $state<{ id: string; summary: string }>();
  let summary = $derived(edit && edit.id === compact?.id ? edit.summary : compact?.summary ?? "");
  let feedback = $derived(view.attempt?.busy ? "Contacting the owner…"
    : view.attempt ? receipts[view.attempt.state] : view.notice ? receipts[view.notice] : undefined);
  let region = $state<HTMLElement>();
  let heading = $state<HTMLHeadingElement>();

  $effect.pre(() => {
    const observed = view;
    const focused = document.activeElement;
    if (!focused || !region?.contains(focused)) return;
    void tick().then(() => {
      if (view === observed && document.activeElement === document.body && !region?.closest("[hidden], [inert]"))
        heading?.focus({ preventScroll: true });
    });
  });

  function decide(accept: boolean) {
    if (!compact || !view.writable) return;
    const reply = !accept ? { kind: "reject_summary" } as const
      : summary === compact.summary ? { kind: "accept_summary" } as const
      : { kind: "edit_summary", summary } as const;
    issue(view, compact.id, "compact_review", reply);
  }
</script>

{#if view.attention || feedback || view.error}
  <section class="interactions" aria-label="Flow requests" bind:this={region}>
    <header>
      <h3 title={view.label} tabindex="-1" bind:this={heading}>Requests</h3>
      {#if view.error}<button type="button" aria-label="Refresh requests" title="Refresh requests" onclick={refresh}>↻</button>{/if}
    </header>
    {#if view.error}<p role="status">Couldn’t read requests.</p>{/if}
    {#if feedback}<p role="status">{feedback}</p>{/if}
    {#if view.attempt && !view.attempt.busy}
      <div class="actions">
        <button type="button" onclick={() => check(view)}>Check status</button>
        {#if view.attempt.state === "unknown"}
          <button type="button" onclick={() => dismissUnknown(view)}>Dismiss status</button>
        {/if}
      </div>
    {/if}
    {#if ready}
      {#each ready.forms as form (`${ready.host_generation}/${form.id}/${form.timing}`)}
        <div class="request">
          <FlowFormReply {form} writable={view.writable}
            issue={(reply) => issue(view, form.id, form.timing === "waiting" ? "waiting_form" : "deferred_form", reply)} />
        </div>
      {/each}
      {#if compact}
        {#key `${ready.host_generation}/${compact.id}`}
          <section class="request compact" aria-label="Compact review">
            <h4>Compact review</h4>
            <p title="Messages included and context tokens before compaction">{compact.slice_count} messages · {compact.tokens_before.toLocaleString()} tokens before</p>
            {#if compact.slice_preview}
              <details><summary>Source preview</summary><pre>{compact.slice_preview}</pre></details>
            {/if}
            <textarea aria-label="Proposed summary" rows="6" spellcheck="false" maxlength="65536"
              value={summary} readonly={!view.writable}
              oninput={(event) => { if (compact) edit = { id: compact.id, summary: event.currentTarget.value }; }}></textarea>
            <div class="actions">
              <button type="button" class="primary" disabled={!view.writable || new TextEncoder().encode(summary).length > 65536}
                onclick={() => decide(true)}>{summary === compact.summary ? "Accept summary" : "Apply edited summary"}</button>
              <button type="button" disabled={!view.writable} onclick={() => decide(false)}>Keep context</button>
            </div>
          </section>
        {/key}
      {/if}
      {#if ready.omitted}<p>Open the Flow in the terminal to review additional requests.</p>{/if}
    {/if}
  </section>
{/if}

<style>
  .interactions { display: grid; gap: .45rem; padding: .7rem .55rem; font-size: .78rem; overflow-wrap: anywhere; }
  header, .actions { display: flex; gap: .35rem; align-items: center; flex-wrap: wrap; }
  header { justify-content: space-between; }
  h3, h4, p { margin: 0; }
  h3, h4 { font-size: inherit; }
  p { color: var(--muted); }
  .request { min-inline-size: 0; border-inline-start: 1px dotted var(--line); padding: .4rem .5rem; }
  .compact { display: grid; gap: .4rem; }
  .compact textarea { min-inline-size: 0; resize: vertical; }
  .actions button, header button { padding: .25rem .55rem; }
  pre { white-space: pre-wrap; }
</style>
