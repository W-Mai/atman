<script lang="ts">
  import type { ActivityView, ExecutionAction } from "./execution-activity.ts";

  let { view, refresh, issue, retry }: {
    view: ActivityView;
    refresh: () => void;
    issue: (view: ActivityView, action: ExecutionAction) => void;
    retry: (view: ActivityView) => void;
  } = $props();

  const phases: Readonly<Record<string, string>> = {
    prepared: "Prepared", idle: "Idle", running: "Running", waiting: "Waiting", finishing: "Finishing",
    cancelling: "Stopping", detached_active: "Execution not attached", interrupted: "Interrupted",
    closing: "Closing", closed: "Closed",
  };
  const interruptions: Readonly<Record<string, string>> = {
    cancelled: "Turn cancelled", failed: "Turn failed", host_lost: "Execution host disconnected",
  };
  let phase = $derived(view.observed?.phase);
  let detail = $derived(view.observed?.interruption == null ? undefined : interruptions[view.observed.interruption]);
  let pending = $derived(view.pending);
  let feedback = $derived(pending?.state === "unknown" ? "Result unknown" : pending?.state === "sending"
    ? pending.action === "cancel_turn" ? "Requesting stop…" : "Requesting recovery…"
    : pending?.state === "accepted" ? pending.action === "cancel_turn" ? "Stopping…" : "Recovering…"
    : view.result === "stopped" ? "Stopped" : view.result === "recovered" ? "Recovered"
    : view.result === "not_sent" ? "Request wasn’t sent" : undefined);
</script>

<section class="execution" aria-label="Flow execution">
  <p class="execution-target" title={`${view.address.session_id} / ${view.address.instance_id}`}>{view.label}</p>
  <div class="execution-state" role="status">
    {#if phase !== undefined}
      <span class="execution-dot" class:active={view.attention} class:interrupted={phase === "interrupted"} aria-hidden="true"></span>
      {phases[phase] ?? "Unknown"}
      {#if view.observed?.paused}<span class="execution-paused">Paused</span>{/if}
    {:else if !view.error}
      <span class="loading-mark" aria-hidden="true"></span> Checking execution…
    {/if}
  </div>
  {#if detail}<p class="execution-secondary">{detail}</p>{/if}
  {#if feedback}<p class="execution-feedback" role="status">{feedback}</p>{/if}
  {#if view.error}<p class="execution-error" role="status">{view.error}</p>{/if}
  <div class="execution-actions">
    {#if view.canControl && view.observed?.allowed_actions.includes("cancel_turn")}
      <button type="button" class="execution-stop" onclick={() => issue(view, "cancel_turn")}>Stop</button>
    {:else if view.canControl && view.observed?.allowed_actions.includes("recover")}
      <button type="button" onclick={() => issue(view, "recover")}>Recover</button>
    {/if}
    {#if pending?.state === "unknown" || view.error}
      <button type="button" onclick={refresh}>Check status</button>
    {/if}
    {#if pending?.retry}
      <button type="button" title="Resend the original request" onclick={() => retry(view)}>Retry request</button>
    {/if}
  </div>
</section>

<style>
  .execution { display: grid; gap: 0.45rem; padding: 0.7rem 0.55rem; font-size: 0.78rem; }
  .execution p { margin: 0; }
  .execution-target { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; }
  .execution-state, .execution-actions { display: flex; align-items: center; gap: 0.45rem; flex-wrap: wrap; }
  .execution-state:empty, .execution-actions:empty { display: none; }
  .execution-dot { width: 0.45rem; height: 0.45rem; border-radius: 50%; background: var(--muted); }
  .execution-dot.active { background: var(--accent); }
  .execution-dot.interrupted, .execution-error { color: var(--danger); }
  .execution-dot.interrupted { background: var(--danger); }
  .execution-paused, .execution-secondary { color: var(--muted); }
  .execution-actions button { padding: 0.25rem 0.55rem; }
  .execution-stop { color: var(--danger); }
</style>
