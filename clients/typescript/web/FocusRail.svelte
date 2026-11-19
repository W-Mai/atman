<script lang="ts">
  import { tick, type Snippet } from "svelte";

  type Surface = "overview" | "activity";
  type Activity = {
    capability: "activity.read";
    state: "idle" | "busy" | "unavailable";
    content: Snippet;
  };

  let { open, drawer, blocked = false, setOpen, overviewContent, activity }: {
    open: boolean;
    drawer: boolean;
    blocked?: boolean;
    setOpen: (open: boolean) => void;
    overviewContent: Snippet;
    activity?: Activity | undefined;
  } = $props();

  let surface = $state<Surface>("overview");
  let openedFrom: Surface = "overview";
  let panel: HTMLDivElement;
  let closeButton: HTMLButtonElement;
  let overviewTrigger: HTMLButtonElement;
  let activityTrigger = $state<HTMLButtonElement | undefined>();
  let activityChoice = $state<HTMLButtonElement | undefined>();
  let activityPane = $state<HTMLDivElement | undefined>();
  let available = $derived(activity?.capability === "activity.read");
  let selected = $derived(available && surface === "activity" ? "activity" : "overview");
  let attention = $derived(activity?.state === "busy" && (!open || selected !== "activity"));
  const id = $props.id();

  $effect.pre(() => {
    if (available) return;
    const focused = document.activeElement;
    const restore = focused === activityTrigger || focused === activityChoice || activityPane?.contains(focused);
    if (surface === "activity") surface = "overview";
    if (restore) void tick().then(() => {
      if (blocked) return;
      if (open && drawer) closeButton?.focus({ preventScroll: true });
      else overviewTrigger?.focus({ preventScroll: true });
    });
  });

  $effect(() => {
    if (open && drawer && !blocked && !panel?.contains(document.activeElement)) {
      closeButton?.focus({ preventScroll: true });
    }
  });

  export async function close(returnFocus = false): Promise<void> {
    const origin = openedFrom;
    setOpen(false);
    if (!returnFocus) return;
    await tick();
    if (!open && !blocked) {
      const trigger = origin === "activity" && available ? activityTrigger : overviewTrigger;
      trigger?.focus({ preventScroll: true });
    }
  }

  async function activate(next: Surface) {
    if (blocked) return;
    if (open && selected === next) return close(true);
    surface = next;
    if (!open || !drawer) openedFrom = next;
    setOpen(true);
    if (!drawer) return;
    await tick();
    if (open && drawer && !blocked && selected === next) closeButton?.focus({ preventScroll: true });
  }

  function trapFocus(event: KeyboardEvent) {
    if (!open || !drawer || blocked || document.querySelector("dialog[open]") || event.key !== "Tab" || event.defaultPrevented) return;
    const items = Array.from(panel.querySelectorAll<HTMLElement>(
      'a[href], button, input, textarea, select, [tabindex]',
    )).filter((item) => item.tabIndex >= 0 && !item.matches(":disabled") &&
      !item.closest("[hidden], [inert]") && item.getClientRects().length > 0);
    const first = items[0];
    const last = items.at(-1);
    if (event.shiftKey && (document.activeElement === first || !panel.contains(document.activeElement))) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && (document.activeElement === last || !panel.contains(document.activeElement))) {
      event.preventDefault();
      first?.focus();
    }
  }
</script>

<svelte:window onkeydown={trapFocus} />

<button type="button" class="drawer-scrim overview-scrim" aria-label="Close focus panel" tabindex="-1" onclick={() => close(true)}></button>

<div bind:this={panel} id={id} class="overview" role={drawer ? "dialog" : "complementary"}
  aria-modal={drawer && open ? true : undefined} aria-labelledby={`${id}-title`} inert={!open || blocked}>
  <div class="overview-header">
    <h2 id={`${id}-title`} class="sidebar-title">{selected === "activity" ? "Activity" : "Overview"}</h2>
    <button bind:this={closeButton} type="button" class="icon overview-close" aria-label="Close focus panel" title="Close" onclick={() => close(true)}>
      <svg viewBox="0 0 18 18" aria-hidden="true"><path d="m5 5 8 8m0-8-8 8"/></svg>
    </button>
  </div>
  {#if available && drawer}
    <div class="panel-choices" role="group" aria-label="Focus panel view">
      <button type="button" aria-pressed={selected === "overview"}
        aria-controls={`${id}-overview`} onclick={() => { surface = "overview"; }}>Overview</button>
      <button bind:this={activityChoice} type="button" aria-pressed={selected === "activity"}
        aria-controls={`${id}-activity`} aria-label={attention ? "Activity · work running" : undefined}
        class="activity-trigger" onclick={() => { surface = "activity"; }}>Activity
        {#if attention}<span class="attention" aria-hidden="true"></span>{/if}
      </button>
    </div>
  {/if}
  <div id={`${id}-overview`} class="focus-content" hidden={selected !== "overview"}>
    {@render overviewContent()}
  </div>
  {#if activity && available}
    <div bind:this={activityPane} id={`${id}-activity`} class="focus-content" hidden={selected !== "activity"}>
      {@render activity.content()}
    </div>
  {/if}
</div>

<div class="focus-rail" data-activity={available || undefined} role="group" aria-label="Focus rail" inert={blocked || (drawer && open)}>
  <button bind:this={overviewTrigger} type="button" class="icon" aria-expanded={open && selected === "overview"}
    aria-controls={id} aria-label="Overview" title="Overview" onclick={() => activate("overview")}>
    <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M6 4.5h8M6 9h8m-8 4.5h8M3.5 4.5h.1M3.5 9h.1m-.1 4.5h.1"/></svg>
  </button>
  {#if available}
    <button bind:this={activityTrigger} type="button" class="icon activity-trigger" aria-expanded={open && selected === "activity"}
      aria-controls={id} aria-label={attention ? "Activity · work running" : "Activity"} title="Activity" onclick={() => activate("activity")}>
      <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M2.5 9h3l2-4.5 2.5 9 2-4.5h3.5"/></svg>
      {#if attention && !(drawer && open)}<span class="attention" aria-hidden="true"></span>{/if}
    </button>
  {/if}
</div>

<style>
  .focus-content { min-height: 0; overflow: auto; overscroll-behavior: contain; }
  .focus-content[hidden] { display: none; }
  .focus-rail[data-activity] { flex-direction: column; align-items: center; justify-content: flex-start; }
  .activity-trigger { position: relative; }
  .attention { position: absolute; inset-inline: 0.22rem; inset-block-end: 0.15rem; block-size: 2px; overflow: hidden; }
  .attention::before { content: ""; display: block; inline-size: 40%; block-size: 100%; background: var(--accent); animation: focus-activity 1.3s ease-in-out infinite alternate; }
  @keyframes focus-activity { from { transform: translateX(0); } to { transform: translateX(150%); } }
  .panel-choices { display: flex; gap: 0.25rem; padding-block: 0.3rem; }
  .panel-choices button { flex: 1; min-height: 2rem; padding-inline: 0.55rem; }
  .panel-choices button[aria-pressed="true"] { color: var(--accent); background: var(--surface); }
  @media (width < 48rem) {
    .focus-rail[data-activity] { flex-direction: row; justify-content: center; width: 5.2rem; }
    :global(.shell:has(.focus-rail[data-activity]) .topbar) { padding-inline-end: 5.2rem; }
  }
</style>
