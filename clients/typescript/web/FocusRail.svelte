<script lang="ts">
  import { tick, type Snippet } from "svelte";

  type Surface = "overview" | "activity";
  type Activity = {
    capability: "activity.read";
    state: "idle" | "busy" | "unavailable";
    responseNeeded?: boolean;
    content: Snippet<[visible: boolean]>;
  };

  let { open, compact, blocked, setOpen, overviewContent, activity, showOverview }: {
    open: boolean;
    compact: boolean;
    blocked: boolean;
    setOpen: (open: boolean) => void;
    overviewContent: Snippet;
    showOverview: (visible: boolean) => void;
    activity?: Activity | undefined;
  } = $props();

  let surface = $state<Surface>("overview");
  let overviewTrigger: HTMLButtonElement;
  let activityTrigger = $state<HTMLButtonElement>();
  let activityPane = $state<HTMLDivElement>();
  let available = $derived(activity?.capability === "activity.read");
  let selected = $derived(available && surface === "activity" ? "activity" : "overview");
  let attention = $derived(activity?.state === "busy" && (!open || selected !== "activity"));
  let activityLabel = $derived(attention ? activity?.responseNeeded ? "Activity · requests" : "Activity · work running" : "Activity");
  const id = $props.id();

  $effect(() => showOverview(open && selected === "overview" && !blocked));

  $effect.pre(() => {
    if (available) return;
    const focused = document.activeElement;
    const restore = focused === activityTrigger || activityPane?.contains(focused);
    if (surface === "activity") surface = "overview";
    if (restore) void tick().then(() => {
      if (!blocked) overviewTrigger?.focus({ preventScroll: true });
    });
  });

  let externalTrigger: HTMLButtonElement | undefined;

  export async function openActivity(trigger?: HTMLButtonElement): Promise<void> {
    if (!available || blocked) return;
    externalTrigger = trigger;
    surface = "activity";
    setOpen(true);
    await tick();
    if (!open || blocked || selected !== "activity" || externalTrigger !== trigger) return;
    const approvals = activityPane?.querySelector<HTMLElement>("[data-approval-region]");
    if (approvals) approvals.focus();
    else activityTrigger?.focus({ preventScroll: true });
  }

  export async function close(returnFocus = false): Promise<void> {
    const origin = selected;
    const source = externalTrigger;
    externalTrigger = undefined;
    setOpen(false);
    if (!returnFocus) return;
    await tick();
    if (!open && !blocked) {
      const trigger = origin === "activity" && available ? activityTrigger : overviewTrigger;
      const restore = source?.isConnected && !source.closest("[hidden], [inert]") ? source : trigger;
      restore?.focus({ preventScroll: true });
    }
  }

  function activate(next: Surface) {
    if (next !== selected) externalTrigger = undefined;
    if (open && selected === next) {
      if (!compact) void close(true);
      return;
    }
    surface = next;
    setOpen(true);
  }
</script>

<div class="focus-rail" inert={blocked}>
  <div class="focus-controls" role="group" aria-label="Focus panel view">
    <button type="button" class="focus-back" hidden={!compact || !open} onclick={() => close(true)}>
      <span aria-hidden="true">←</span> Back
    </button>
    <button bind:this={overviewTrigger} type="button" class="focus-trigger" aria-expanded={open && selected === "overview"}
      aria-controls={`${id}-overview`} aria-label="Overview" title="Overview" onclick={() => activate("overview")}>
      <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M6 4.5h8M6 9h8m-8 4.5h8M3.5 4.5h.1M3.5 9h.1m-.1 4.5h.1"/></svg>
      <span class="focus-label">Overview</span>
    </button>
    {#if available}
      <button bind:this={activityTrigger} type="button" class="focus-trigger" aria-expanded={open && selected === "activity"}
        aria-controls={`${id}-activity`} aria-label={activityLabel} title={activityLabel} onclick={() => activate("activity")}>
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M2.5 9h3l2-4.5 2.5 9 2-4.5h3.5"/></svg>
        <span class="focus-label">Activity</span>
        {#if attention}<span class="attention" aria-hidden="true"></span>{/if}
      </button>
    {/if}
  </div>

  <div class="overview" hidden={!open}>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (Allow keyboard scrolling of the independent region.) -->
    <div id={`${id}-overview`} class="focus-content" role="region" aria-label="Overview" tabindex="0" hidden={selected !== "overview"}>
      {@render overviewContent()}
    </div>
    {#if activity && available}
      <div bind:this={activityPane} id={`${id}-activity`} class="focus-content" role="region" aria-label="Activity" hidden={selected !== "activity"}>
        {@render activity.content(open && selected === "activity" && !blocked)}
      </div>
    {/if}
  </div>
</div>

<style>
  .focus-content { min-height: 0; height: 100%; overflow: auto; overscroll-behavior: contain; }
  .attention { position: absolute; inset-inline: 0.3rem; inset-block-end: 0.17rem; block-size: 2px; overflow: hidden; }
  .attention::before { content: ""; display: block; inline-size: 40%; block-size: 100%; background: var(--accent); animation: focus-activity 1.3s ease-in-out infinite alternate; }
  @keyframes focus-activity { from { transform: translateX(0); } to { transform: translateX(150%); } }
</style>
