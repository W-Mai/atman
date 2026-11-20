<script module lang="ts">
  import type { Dtos } from "../src/index.ts";

  import type { OverviewView } from "./flow-overview.ts";
</script>

<script lang="ts">
  import type { Snippet } from "svelte";

  let { view, refresh, flows }: {
    view: OverviewView | undefined;
    refresh: () => void;
    flows?: Snippet;
  } = $props();

  const todoMark = {
    pending: "○", in_progress: "◐", done: "✓", cancelled: "×",
  } satisfies Record<Dtos["OverviewTodoStatus"], string>;
  const todoState = {
    pending: "Pending", in_progress: "In progress", done: "Done", cancelled: "Cancelled",
  } satisfies Record<Dtos["OverviewTodoStatus"], string>;
  const pauseReason = {
    user_requested: "Paused by user", parent_requested: "Paused by parent", completed: "Completed",
    needs_input: "Needs input", blocked: "Blocked", cancelled: "Cancelled", run_failed: "Run failed",
    interrupted: "Interrupted",
  } satisfies Record<Dtos["OverviewGoalPauseReason"], string>;
  let snapshot = $derived(view?.snapshot);
  let plan = $derived(snapshot?.planning.plan);
  let count = $derived(snapshot?.planning.todo_counts);
  let total = $derived(count ? count.pending + count.in_progress + count.done + count.cancelled : 0);
</script>

{#snippet text(value: Dtos["OverviewText"])}
  {value.text}{#if value.truncated}<span class="muted" title="Truncated by the host"><span aria-hidden="true">…</span><span class="sr-only"> (truncated)</span></span>{/if}
{/snippet}

{#if view}
  {#key view.key}
    {#if snapshot?.goal || plan || total > 0}
      <div class="overview-planning">
        {#if snapshot?.goal}
          {@const goal = snapshot.goal}
          <details class="overview-section" open>
            <summary><span>Goal</span><span class="meter" class:active={goal.status === "active"}>
              {goal.status === "active" ? "Active" : goal.pause_reason ? pauseReason[goal.pause_reason] : "Paused"}
            </span></summary>
            <p class="goal-text">{@render text(goal.objective)}</p>
          </details>
        {/if}
        {#if plan}
          <details class="overview-section" open>
            <summary><span>Plan</span><span class="meter" aria-label={plan.done + " of " + plan.total + " steps done"}>{plan.done}/{plan.total}</span></summary>
            <p class="plan-title">{@render text(plan.title)}</p>
            <ol class="steps">
              {#each plan.steps as step (step.index)}
                <li class:done={step.done}>
                  <span aria-hidden="true" class="mark">{step.done ? "✓" : "○"}</span>
                  <span class="sr-only">{step.done ? "Done: " : "Pending: "}</span>
                  <span class="step-index" aria-hidden="true">{step.index + 1}.</span>
                  <span>{@render text(step.text)}</span>
                </li>
              {/each}
            </ol>
            {#if plan.steps_truncated}<p class="muted limit">Showing {plan.steps.length} of {plan.total} steps</p>{/if}
          </details>
        {/if}
        {#if total > 0}
          <details class="overview-section" open>
            <summary><span>Todos</span><span class="meter" aria-label={(count?.done ?? 0) + " of " + total + " tasks done"}>{count?.done ?? 0}/{total}</span></summary>
            <ul class="todos">
              {#each snapshot?.planning.todos ?? [] as todo (todo.id)}
                <li>
                  <details>
                    <summary>
                      <span class="todo-label">
                        <span class="mark" data-status={todo.status} aria-hidden="true">{todoMark[todo.status]}</span>
                        <span class="sr-only">{todoState[todo.status]}: </span>
                        <span>{#if todo.why.text}{@render text(todo.why)}{:else}{@render text(todo.where)}{/if}</span>
                      </span>
                    </summary>
                    <dl>
                      {#if todo.where.text}<dt>Where</dt><dd>{@render text(todo.where)}</dd>{/if}
                      {#if todo.how.text}<dt>How</dt><dd>{@render text(todo.how)}</dd>{/if}
                      {#if todo.expected_result.text}<dt>Expected</dt><dd>{@render text(todo.expected_result)}</dd>{/if}
                    </dl>
                  </details>
                </li>
              {/each}
            </ul>
            {#if snapshot?.planning.todos_truncated}<p class="muted limit">Showing {snapshot.planning.todos.length} of {total} tasks</p>{/if}
          </details>
        {/if}
      </div>
    {/if}
    {#if view.failed}
      <div class="overview-feedback">
        <span role="status">{snapshot ? "Overview could not refresh" : "Overview unavailable"}</span>
        <button type="button" onclick={refresh}>Retry</button>
      </div>
    {:else if view.reading && !snapshot}
      <p class="overview-feedback" role="status"><span class="loading-mark" aria-hidden="true"></span><span class="sr-only">Loading overview</span></p>
    {/if}
  {/key}
{/if}

{@render flows?.()}

{#if snapshot?.workspace || snapshot?.project_root}
  <section class="workspace-summary" aria-label="Workspace">
    <h3>Workspace</h3>
    {#if snapshot.workspace}
      <p class="workspace-path">{@render text(snapshot.workspace.path)}</p>
      <p class="muted">{snapshot.workspace.kind === "allocate" ? "Recorded · allocation" : "Recorded · shared"}</p>
    {:else if snapshot.project_root}
      <p class="workspace-path">{@render text(snapshot.project_root)}</p>
      <p class="muted">Project root</p>
    {/if}
  </section>
{/if}

<style>
  .overview-planning { display: grid; gap: .3rem; margin-block: .3rem; }
  .overview-section, .workspace-summary { background: var(--surface); min-inline-size: 0; }
  summary { list-style: none; cursor: pointer; }
  summary::-webkit-details-marker { display: none; }
  summary:focus-visible { outline: 1px solid var(--accent); outline-offset: -1px; }
  summary:hover { background: var(--hover); }
  .overview-section > summary {
    display: grid;
    grid-template-columns: .65rem minmax(0, 1fr) auto;
    align-items: center;
    gap: .35rem;
    padding: .43rem .65rem;
    color: var(--accent);
    font-size: .8rem;
    font-weight: 600;
  }
  .overview-section > summary::before { content: "›"; text-align: center; }
  .overview-section[open] > summary::before { transform: rotate(90deg); }
  .meter, .muted, .step-index, dt { color: var(--muted); }
  .meter { font-weight: 400; font-variant-numeric: tabular-nums; text-align: end; }
  .meter.active { color: var(--accent); }
  p { margin: 0; }
  .goal-text, .plan-title, .limit { padding: .15rem .65rem .5rem 1.65rem; }
  .goal-text, .plan-title, li, .workspace-summary, .overview-feedback, .limit { font-size: .78rem; overflow-wrap: anywhere; }
  .goal-text, li, dd { white-space: pre-wrap; }
  .plan-title { font-weight: 600; }
  .steps, .todos { list-style: none; padding: 0 .65rem .35rem 1.65rem; margin: 0; }
  .steps li { display: grid; grid-template-columns: .9rem max-content minmax(0, 1fr); align-items: baseline; gap: .25rem; padding: .14rem 0; }
  .steps li.done { color: var(--muted); }
  .mark { text-align: center; color: var(--muted); }
  .done .mark, .mark[data-status="done"] { color: var(--success, var(--accent)); }
  .mark[data-status="in_progress"] { color: var(--warn, var(--accent)); }
  .todo-label { display: grid; grid-template-columns: .9rem minmax(0, 1fr); gap: .35rem; align-items: baseline; }
  .todos summary { display: grid; grid-template-columns: minmax(0, 1fr) .65rem; gap: .2rem; align-items: baseline; padding: .18rem .25rem; margin-inline: -.25rem; }
  .todos summary::after { content: "›"; color: var(--muted); text-align: center; }
  .todos details[open] > summary::after { transform: rotate(90deg); }
  .todos dl { display: grid; gap: .15rem; margin: .25rem 0 .45rem 1.25rem; }
  .todos dd { margin: 0 0 .14rem; }
  .workspace-summary { display: grid; gap: .16rem; padding: .5rem .65rem; margin-block: .3rem; }
  .workspace-summary h3 { margin: 0; font-size: .8rem; font-weight: 600; }
  .workspace-path { font-family: var(--atman-mono, monospace); }
  .overview-feedback { display: flex; align-items: center; gap: .4rem; padding: .45rem .65rem; color: var(--muted); }
  .overview-feedback button { padding: .18rem .42rem; }
  .sr-only { position: absolute; inline-size: 1px; block-size: 1px; overflow: hidden; clip-path: inset(50%); white-space: nowrap; }
</style>
