<script lang="ts">
  import { tick, type Snippet } from "svelte";

  type Surface = "overview" | "activity";
  type Activity = {
    capability: "activity.read";
    state: "idle" | "busy" | "unavailable";
    content: Snippet;
  };

  let { open, compact, blocked, setOpen, overviewContent, activity }: {
    open: boolean;
    compact: boolean;
    blocked: boolean;
    setOpen: (open: boolean) => void;
    overviewContent: Snippet;
    activity?: Activity | undefined;
  } = $props();

  let surface = $state<Surface>("overview");
  let overviewTrigger: HTMLButtonElement;
  let activityTrigger = $state<HTMLButtonElement>();
  let activityPane = $state<HTMLDivElement>();
  let available = $derived(activity?.capability === "activity.read");
  let selected = $derived(available && surface === "activity" ? "activity" : "overview");
  let attention = $derived(activity?.state === "busy" && (!open || selected !== "activity"));
  const id = $props.id();

  $effect.pre(() => {
    if (available) return;
    const focused = document.activeElement;
    const restore = focused === activityTrigger || activityPane?.contains(focused);
    if (surface === "activity") surface = "overview";
    if (restore) void tick().then(() => {
      if (!blocked) overviewTrigger?.focus({ preventScroll: true });
    });
  });

  export async function close(returnFocus = false): Promise<void> {
    const origin = selected;
    setOpen(false);
    if (!returnFocus) return;
    await tick();
    if (!open && !blocked) {
      const trigger = origin === "activity" && available ? activityTrigger : overviewTrigger;
      trigger?.focus({ preventScroll: true });
    }
  }

  function activate(next: Surface) {
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
        aria-controls={`${id}-activity`} aria-label={attention ? "Activity · work running" : "Activity"} title="Activity" onclick={() => activate("activity")}>
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M2.5 9h3l2-4.5 2.5 9 2-4.5h3.5"/></svg>
        <span class="focus-label">Activity</span>
        {#if attention}<span class="attention" aria-hidden="true"></span>{/if}
      </button>
    {/if}
  </div>

  <div class="overview" hidden={!open}>
    <div id={`${id}-overview`} class="focus-content" role="region" aria-label="Overview" hidden={selected !== "overview"}>
      {@render overviewContent()}
    </div>
    {#if activity && available}
      <div bind:this={activityPane} id={`${id}-activity`} class="focus-content" role="region" aria-label="Activity" hidden={selected !== "activity"}>
        {@render activity.content()}
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
