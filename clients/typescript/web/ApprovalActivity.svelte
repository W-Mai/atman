<script lang="ts">
  import { tick } from "svelte";
  import type { ApprovalAction, ApprovalSelection, PendingApproval } from "../src/approvals.ts";
  import type { ApprovalView } from "./approval-activity.ts";

  let { view, refresh, review, dismiss, issue, check, dismissPending }: {
    view: ApprovalView;
    refresh: () => void;
    review: (view: ApprovalView, selection: ApprovalSelection) => boolean;
    dismiss: (view: ApprovalView) => void;
    issue: (view: ApprovalView, action: ApprovalAction) => void;
    check: (view: ApprovalView) => void;
    dismissPending: (view: ApprovalView) => void;
  } = $props();

  const risks: Readonly<Record<string, string>> = {
    workspace_external: "Outside workspace", network: "Network", irreversible: "Irreversible",
    filesystem_write: "File write", process_spawn: "Process", repository_mutation: "Repository change",
  };
  const results: Readonly<Record<ApprovalAction, string>> = {
    allow_once: "Approval recorded · Call may now execute",
    allow_tool: "Same-tool approval recorded · Current run only",
    allow_path: "Same-path approval recorded · Current run only",
    deny: "Denial recorded",
    cancel_request: "Approval request cancelled",
  };
  const label = (row: PendingApproval) => row.review.intent || row.tool || "Tool call";
  let requests = $derived(view.snapshot?.requests ?? []);
  let groups = $derived(view.snapshot?.groups ?? []);
  let selected = $derived(view.review);
  let single = $derived(selected?.selection.kind === "requests" ? selected.requests[0] : undefined);
  let scope = $state<{ key: string; action: "allow_tool" | "allow_path" }>();
  let allowAction: ApprovalAction = $derived(!view.currentCallOnly && scope && scope.key === `${view.key}/${selected?.revisionKey}` &&
    (scope.action === "allow_path" ? single?.samePathGrant : single?.sameToolGrant) ? scope.action : "allow_once");
  let incomplete = $derived(selected?.requests.some(({ review }) => review.incomplete) === true);
  let feedback = $derived(view.sending ? "Sending decision…"
    : view.checking ? "Checking decision…"
    : view.pending ? "Decision unconfirmed."
    : view.notice?.state === "settled" ? results[view.notice.receipt.action]
    : view.notice?.state === "unknown" ? "Decision unconfirmed. Review the current queue."
    : view.notice?.state === "not_sent" ? "Decision wasn’t sent."
    : view.notice?.state === "capacity" ? "Check earlier decisions before sending another."
    : undefined);
  let failure = $derived(view.failed ? "Can’t read approvals."
    : view.snapshot?.state === "capacity" ? "This queue exceeds the Web review limit."
    : view.snapshot?.state === "unavailable" ? "The Flow owner is unavailable."
    : undefined);
  let empty = $derived(view.currentCallOnly && view.snapshot?.state === "ready" && !requests.length && !feedback && !failure);
  let reviewBox: HTMLElement | undefined = $state();
  let reviewHeading: HTMLHeadingElement | undefined = $state();
  let trigger: HTMLButtonElement | undefined;
  let focusGeneration = 0;

  function findTrigger(): HTMLButtonElement | undefined {
    return trigger?.isConnected && !trigger.closest("[hidden], [inert]") ? trigger : undefined;
  }

  async function show(selection: ApprovalSelection, button: HTMLButtonElement) {
    if (!review(view, selection)) return;
    scope = undefined;
    trigger = button;
    const generation = ++focusGeneration;
    const key = view.key;
    await tick();
    if (generation === focusGeneration && key === view.key && selected)
      reviewHeading?.focus({ preventScroll: true });
  }

  async function close() {
    scope = undefined;
    const original = view;
    dismiss(original);
    const generation = ++focusGeneration;
    await tick();
    if (generation === focusGeneration && view.key === original.key && !selected) findTrigger()?.focus({ preventScroll: true });
  }

  $effect.pre(() => {
    if (selected || !reviewBox?.contains(document.activeElement)) return;
    const focused = document.activeElement;
    const key = view.key;
    const generation = focusGeneration;
    void tick().then(() => {
      // Preserve focus only when this review disappeared while the user was inside it.
      if (generation === focusGeneration && key === view.key && !selected &&
        (document.activeElement === focused || document.activeElement === document.body)) {
        findTrigger()?.focus({ preventScroll: true });
      }
    });
  });
</script>

{#if requests.length || feedback || failure || empty || (view.reading && !view.snapshot)}
  <section class="approval-activity" aria-label="Approvals" data-approval-region tabindex="-1" aria-busy={view.sending || view.checking || view.reading}>
    <header>
      <h3 title={view.label}><span aria-hidden="true">⚑</span> Approvals{#if requests.length}<span class="count">{requests.length}</span>{/if}</h3>
      {#if failure || (!view.pending && view.notice?.state === "unknown") || view.notice?.state === "not_sent"}
        <button type="button" class="queue-refresh" aria-label="Refresh approvals" title="Refresh approvals" onclick={refresh}>↻</button>
      {/if}
    </header>
    {#if failure}<p class="feedback" role="status">{failure}</p>{/if}
    {#if feedback}<p class="feedback" role="status">{feedback}</p>{/if}
    {#if empty}<p class="feedback">No pending approvals.</p>{/if}
    {#if view.pending && !view.sending && !view.checking}
      <div class="decision-bar">
        <button type="button" onclick={() => check(view)}>Check receipt</button>
        <button type="button" aria-label="Stop checking this receipt" title="Stop checking this receipt" onclick={() => dismissPending(view)}>×</button>
      </div>
    {/if}
    {#if !view.snapshot && view.reading && !view.sending && !view.checking}<p class="feedback"><span class="loading-mark" aria-hidden="true"></span> Checking approvals…</p>{/if}
    {#if requests.length}
      <ul class="approval-queue" aria-label="Pending requests and groups">
        {#each groups as group (group.id)}
          <li>
            <button type="button" class="review-button" title={group.label || "Approval group"}
              aria-pressed={selected?.selection.kind === "group" && selected.selection.id === group.id}
              aria-disabled={view.failed}
              onclick={(event) => show({ kind: "group", id: group.id }, event.currentTarget)}>
              <span class="row-mark" aria-hidden="true">⊞</span><span class="row-title">{group.label || "Group"}</span><span class="row-meta">{group.members.length}</span>
            </button>
          </li>
        {/each}
        {#each requests as request (request.id)}
          <li>
            <button type="button" class="review-button" title={label(request)}
              aria-pressed={selected?.selection.kind === "requests" && selected.selection.ids[0] === request.id}
              aria-disabled={view.failed}
              onclick={(event) => show({ kind: "requests", ids: [request.id] }, event.currentTarget)}>
              <span class="row-mark" aria-hidden="true">›</span><span class="row-title">{label(request)}</span>
              {#if request.review.execution_boundary === "direct"}<span class="row-meta direct">Direct</span>{/if}
            </button>
          </li>
        {/each}
      </ul>
    {/if}
    {#if selected}
      <div class="approval-review" bind:this={reviewBox}>
        <div class="review-heading">
          <h4 tabindex="-1" bind:this={reviewHeading}>{selected.label}</h4>
          <button type="button" aria-label="Hide approval review" title="Hide review" onclick={close}>×</button>
        </div>
        {#each selected.requests as request (request.id)}
          {@const detail = request.review}
          <div class="request-detail">
            <p class="tool-name" title={request.toolUseId ?? request.id}>{request.tool || "Tool call"}</p>
            {#if detail.execution_boundary === "direct"}<p class="direct">Runs directly on the host.</p>
            {:else if detail.execution_boundary === "sandboxed"}<p>Runs in the sandbox.</p>{/if}
            {#if detail.risks.length || detail.network}
              <ul class="risk-list" aria-label="Risks">
                {#each detail.risks as risk}<li>{risks[risk] ?? risk}</li>{/each}
                {#if detail.network && !detail.risks.includes("network")}<li>Network</li>{/if}
              </ul>
            {/if}
            {#if detail.preview}<pre>{detail.preview}</pre>{/if}
            {#if detail.cwd}<p class="resource"><span>Directory</span><code>{detail.cwd}</code></p>{/if}
            {#if detail.targets.some((target) => target !== detail.cwd)}
              <ul class="targets" aria-label="Targets">
                {#each detail.targets.filter((target) => target !== detail.cwd) as target}<li><code>{target}</code></li>{/each}
              </ul>
            {/if}
          </div>
        {/each}
        {#if incomplete}<p class="feedback">Review the complete request in the terminal before allowing it.</p>{/if}
        {#if !view.writable}<p class="feedback">This owner provides read-only review.</p>{/if}
        <div class="approval-scope" aria-label="Approval scope">
          {#if !view.currentCallOnly && single?.sameToolGrant && selected && !incomplete && view.writable}
            {@const key = `${view.key}/${selected.revisionKey}`}
            <button type="button" aria-pressed={allowAction === "allow_once"} onclick={() => scope = undefined}>Current call</button>
            <button type="button" aria-pressed={allowAction === "allow_tool"}
              title="Same tool in this run, within this grant’s authority"
              onclick={() => scope = { key, action: "allow_tool" }}>Same tool</button>
            {#if single.samePathGrant}
              <button type="button" aria-pressed={allowAction === "allow_path"}
                title="Same tool and workspace path in this run, within this grant’s authority"
                onclick={() => scope = { key, action: "allow_path" }}>Same path</button>
            {/if}
          {:else}
            <span title="Only the pending selection" class="scope-current">Current call</span>
          {/if}
        </div>
        <div class="decision-bar" aria-label="Decide approval">
          <button type="button" class="allow" aria-disabled={!view.canDecide || incomplete}
            onclick={() => issue(view, allowAction)}>{allowAction === "allow_once" ? "Allow once"
              : allowAction === "allow_tool" ? "Allow same tool" : "Allow same path"}</button>
          <button type="button" class="deny" aria-disabled={!view.canDecide}
            onclick={() => issue(view, "deny")}>Deny</button>
          <button type="button" aria-disabled={!view.canDecide}
            onclick={() => issue(view, "cancel_request")}>Cancel request</button>
        </div>
      </div>
    {/if}
  </section>
{/if}

<style>
  .approval-activity { display: grid; gap: .55rem; border-block-start: 1px solid var(--line); padding: .7rem .55rem; font-size: .78rem; }
  p, h3, h4 { margin: 0; }
  h3, h4 { font-size: inherit; }
  header, .review-heading { display: flex; align-items: center; justify-content: space-between; gap: .45rem; min-inline-size: 0; }
  h3 span:first-child { color: var(--accent); margin-inline-end: .2rem; }
  .count { margin-inline-start: .45rem; color: var(--muted); font-family: var(--atman-mono); }
  ul { list-style: none; padding: 0; margin: 0; }
  .approval-queue { display: grid; gap: .12rem; }
  button { padding: .25rem .5rem; }
  .review-button { display: flex; align-items: center; inline-size: 100%; text-align: start; gap: .35rem; border-color: transparent; }
  .review-button[aria-pressed="true"] { background: var(--surface); color: var(--accent); border-inline-start-color: var(--accent); }
  .row-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row-meta { margin-inline-start: auto; color: var(--muted); }
  .row-mark, .feedback { color: var(--muted); }
  .approval-review { display: grid; gap: .6rem; min-inline-size: 0; border-inline-start: 1px solid var(--line); padding-inline-start: .55rem; }
  .review-heading h4 { overflow-wrap: anywhere; }
  .review-heading button, .queue-refresh { flex: none; border-color: transparent; }
  .request-detail { min-inline-size: 0; display: grid; gap: .3rem; }
  .request-detail + .request-detail { border-block-start: 1px dashed var(--line); padding-block-start: .45rem; }
  .direct, .deny { color: var(--danger); }
  .tool-name, code, pre { font-family: var(--atman-mono); overflow-wrap: anywhere; }
  .tool-name, .resource > span { color: var(--muted); }
  .resource code { display: block; }
  pre { margin: 0; padding: .35rem .45rem; background: var(--surface); white-space: pre-wrap; font-size: inherit; }
  .risk-list, .approval-scope, .decision-bar { display: flex; align-items: center; flex-wrap: wrap; gap: .28rem; }
  .risk-list li { border: 1px solid var(--line); padding: .1rem .34rem; }
  .approval-scope { font-size: .73rem; }
  .approval-scope button[aria-pressed="true"] { color: var(--accent); border-color: var(--accent); }
  .scope-current { color: var(--accent); padding-inline: .3rem; }
  .allow { color: var(--accent); }
  button[aria-disabled="true"] { color: var(--muted); opacity: .55; cursor: default; }
</style>
