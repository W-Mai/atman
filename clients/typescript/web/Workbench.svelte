<script lang="ts">
  import { onMount, tick } from "svelte";
  import logo from "../../../static/ATMAN-LOGO.png";
  import FlowMap from "./FlowMap.svelte";
  import type { FlowMapView } from "./flow-map.ts";
  import OverviewPanel from "./OverviewPanel.svelte";
  import type { OverviewView } from "./flow-overview.ts";
  import FocusRail from "./FocusRail.svelte";
  import ExecutionPanel from "./ExecutionPanel.svelte";
  import ApprovalActivity from "./ApprovalActivity.svelte";
  import WorkUsage from "./WorkUsage.svelte";
  import WorkLog from "./WorkLog.svelte";
  import type { UsageObservation } from "./work-usage.ts";
  import type { LogObservation } from "./work-log.ts";
  import type { ApprovalAction, ApprovalSelection } from "../src/approvals.ts";
  import type { ApprovalView } from "./approval-activity.ts";
  import type { ActivityView, ExecutionAction } from "./execution-activity.ts";
  import type { WorkbenchElements } from "./shell-elements.ts";

  let { onReady }: { onReady: (elements: WorkbenchElements) => void | (() => void) } = $props();
  let shell!: HTMLElement;
  let sidebarToggle!: HTMLButtonElement;
  let connectionState!: HTMLElement;
  let sidebar!: HTMLElement;
  let sessions!: HTMLElement;
  let search!: HTMLInputElement;
  let targetTitle!: HTMLElement;
  let tabs!: HTMLElement;
  let content!: HTMLElement;
  let composer!: HTMLElement;
  let status!: HTMLElement;
  let disconnectButton!: HTMLButtonElement;
  let settingsButton!: HTMLButtonElement;
  let forkStatusButton!: HTMLButtonElement;
  let newProjectButton!: HTMLButtonElement;
  let refreshButton!: HTMLButtonElement;
  let focusRail: FocusRail | undefined = $state();
  let overview = $state.raw<OverviewView | undefined>();
  let overviewActions: Parameters<WorkbenchElements["bindOverviewActions"]>[0] | undefined;
  function showOverview(visible: boolean) { overviewActions?.visible(visible); }
  function refreshOverview() { overviewActions?.refresh(); }
  let activity = $state.raw<ActivityView | undefined>();
  let usage = $state.raw<UsageObservation | undefined>();
  let logs = $state.raw<LogObservation | undefined>();
  let approvals = $state.raw<ApprovalView | undefined>();
  let approvalActions: Parameters<WorkbenchElements["bindApprovalActions"]>[0] | undefined;
  function reviewApproval(view: ApprovalView, selection: ApprovalSelection) {
    return approvalActions?.review(view, selection) ?? false;
  }
  function dismissApproval(view: ApprovalView) { approvalActions?.dismiss(view); }
  function issueApproval(view: ApprovalView, action: ApprovalAction) { approvalActions?.issue(view, action); }
  function refreshApprovals() { approvalActions?.refresh(); }
  let activityActions: Parameters<WorkbenchElements["bindActivityActions"]>[0] | undefined;

  function refreshActivity() { activityActions?.refresh(); }
  function issue(view: ActivityView, action: ExecutionAction) { activityActions?.issue(view, action); }
  function retry(view: ActivityView) { activityActions?.retry(view); }
  let overviewOpen = $state(false);
  let projectsDrawer = $state(false);
  let compactFocus = $state(false);
  let map = $state.raw<FlowMapView | undefined>();
  let mapOpen = $state(false);
  let mapBlocked = $state(false);
  let mapToggle = $state<HTMLButtonElement>();

  async function toggleFlowMap() {
    mapOpen = !mapOpen;
    if (!mapOpen) return;
    map?.refresh();
    await tick();
    if (mapOpen) shell.querySelector<HTMLElement>(".flow-map")?.focus({ preventScroll: true });
  }

  function presentFlowMap(view: FlowMapView | undefined) {
    map = view;
    if (!view) mapOpen = false;
  }


  onMount(() => {
    const cleanup = onReady({
      shell, sidebarToggle, connectionState, sidebar, sessions, search, targetTitle,
      tabs, content, composer, status, disconnectButton, settingsButton, forkStatusButton, newProjectButton, refreshButton,
      showConversationTarget: (target) => { void returnToConversation(target); },
      presentFlowMap,
      presentOverview: (view) => { overview = view; },
      bindOverviewActions: (actions) => { overviewActions = actions; },
      presentActivity: (view) => { activity = view; },
      presentUsage: (observation) => { usage = observation; },
      presentLogs: (observation) => { logs = observation; },
      bindActivityActions: (actions) => { activityActions = actions; },
      openApprovals: (view, trigger) => {
        if (approvals === view && view.attention) void focusRail?.openActivity(trigger);
      },
      presentApprovals: (view) => { approvals = view; },
      bindApprovalActions: (actions) => { approvalActions = actions; },
    });
    const projectsMedia = matchMedia("(width < 48rem)");
    const focusMedia = matchMedia("(width < 64rem)");
    function syncLayout() {
      projectsDrawer = projectsMedia.matches && shell.classList.contains("sidebar-open");
      compactFocus = focusMedia.matches;
      mapBlocked = tabs.inert;
      if (projectsDrawer) overviewOpen = false;
    }
    const observer = new MutationObserver(syncLayout);
    observer.observe(sidebarToggle, { attributeFilter: ["aria-expanded"] });
    observer.observe(tabs, { attributeFilter: ["inert"] });
    async function returnToConversation(target: HTMLElement) {
      const wasMap = mapOpen;
      mapOpen = false;
      if (compactFocus && overviewOpen) await focusRail?.close();
      else if (!wasMap) return;
      await tick();
      if ((compactFocus && overviewOpen) || projectsDrawer || !target.isConnected) return;
      target.focus({ preventScroll: true });
    }
    projectsMedia.addEventListener("change", syncLayout);
    focusMedia.addEventListener("change", syncLayout);
    syncLayout();
    return () => {
      observer.disconnect();
      projectsMedia.removeEventListener("change", syncLayout);
      focusMedia.removeEventListener("change", syncLayout);
      cleanup?.();
    };
  });

  function revealDesktopProjects() {
    sidebarToggle.click();
    search.focus({ preventScroll: true });
  }

  function projectsOpen() {
    return projectsDrawer;
  }

  function closeProjects() {
    if (!projectsOpen()) return;
    sidebarToggle.click();
    sidebarToggle.focus();
  }

  function setFocusPanelOpen(open: boolean) {
    if (open && projectsOpen()) closeProjects();
    overviewOpen = open;
  }

  function handleEscape(event: KeyboardEvent) {
    if (event.key !== "Escape" || event.defaultPrevented || event.isComposing || document.querySelector("dialog[open]")) return;
    if (projectsOpen()) closeProjects();
    else if (overviewOpen) void focusRail?.close(true);
    else if (mapOpen) { mapOpen = false; mapToggle?.focus(); }
    else return;
    event.preventDefault();
  }
</script>

<svelte:window onkeydown={handleEscape} />

<div class="shell" class:overview-open={overviewOpen} bind:this={shell}>
  <header class="topbar">
    <div class="brand">
      <button bind:this={sidebarToggle} type="button" class="icon sidebar-toggle" aria-controls="session-sidebar" aria-expanded="true" aria-label="Projects" title="Projects">
        <svg viewBox="0 0 18 18" aria-hidden="true"><rect x="2.5" y="3" width="13" height="12" rx="1"/><path d="M7 3v12"/></svg>
      </button>
      <img class="brand-logo" src={logo} alt="" width="24" height="24" />
      <span>atman</span>
    </div>
    <h1 bind:this={targetTitle} class="target-title">No Session selected</h1>
    <div class="connection">
      {#if map}
        <button bind:this={mapToggle} type="button" class="icon map-toggle" aria-pressed={mapOpen}
          aria-controls="flow-map-workspace" aria-label={mapOpen ? "Show Transcript" : "Show Flow Map"}
          title={mapOpen ? "Show Transcript" : "Show Flow Map"}
          inert={projectsDrawer || (overviewOpen && compactFocus)} onclick={toggleFlowMap}>
          <svg viewBox="0 0 18 18" aria-hidden="true">
            {#if mapOpen}<path d="M4 5h10M4 9h10M4 13h7" />
            {:else}<path d="M5 4H3v9h2m0-4H3m4-5h7M7 9h7m-7 4h7" />{/if}
          </svg>
        </button>
      {/if}
      <span bind:this={connectionState} class="connection-state">Disconnected</span>
      <button bind:this={forkStatusButton} type="button" class="icon workspace-pending" aria-label="Resume Session Fork"
        title="Resume Session Fork" aria-haspopup="dialog" hidden inert={projectsDrawer || (overviewOpen && compactFocus)}>⑂</button>
      <button bind:this={settingsButton} type="button" class="icon settings-toggle" aria-label="Settings" title="Settings"
        aria-haspopup="dialog" hidden inert={projectsDrawer || (overviewOpen && compactFocus)}>
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M3 5h12M3 9h12M3 13h12M6 3.5v3M12 7.5v3M8 11.5v3"/></svg>
      </button>
      <button bind:this={disconnectButton} type="button" class="icon" aria-label="Disconnect" title="Disconnect">
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M8 3.5H3.5v11H8m1-5.5h6m-2.5-2.5L15 9l-2.5 2.5"/></svg>
      </button>
    </div>
  </header>

  <nav bind:this={sidebar} class="sidebar" id="session-sidebar" aria-label="Projects">
    <div class="sidebar-tools">
      <h2 class="sidebar-title">Projects</h2>
      <button bind:this={newProjectButton} type="button" class="icon" aria-label="New Project" title="New Project" aria-haspopup="dialog" hidden>
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M9 4v10M4 9h10"/></svg>
      </button>
      <button bind:this={refreshButton} type="button" class="icon" aria-label="Refresh Projects and Sessions" title="Refresh Projects and Sessions">
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M14.5 8a5.5 5.5 0 1 0-.75 4m.75-8v4h-4"/></svg>
      </button>
    </div>
    <input bind:this={search} class="session-search" type="search" aria-label="Find project or session" placeholder="Find…" />
    <div bind:this={sessions} class="session-list"></div>
  </nav>

  <div class="projects-reveal">
    <button type="button" class="icon" aria-controls="session-sidebar" aria-expanded="false"
      aria-label="Show Projects" title="Show Projects" onclick={revealDesktopProjects}>P</button>
  </div>

  <main class="workspace" aria-labelledby="workbench-transcript" inert={projectsDrawer || (overviewOpen && compactFocus)}>
    <h2 class="visually-hidden" id="workbench-transcript">Transcript</h2>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (The independent scroll region must support keyboard scrolling.) -->
    <div bind:this={content} class="content" role="region" aria-labelledby="workbench-transcript" tabindex="0" class:map-transcript-hidden={mapOpen} inert={mapOpen}></div>
    <div class="map-workspace" id="flow-map-workspace" hidden={!mapOpen}>
      {#if map && mapOpen}<FlowMap {...map} pending={mapBlocked} />{/if}
    </div>
    <div bind:this={status} class="status-line" role="status"></div>
    <div bind:this={composer} class="composer-slot"></div>
  </main>

  <button type="button" class="drawer-scrim projects-scrim" aria-label="Close Projects" tabindex="-1" onclick={closeProjects}></button>
  {#snippet overviewFlows()}
    <nav bind:this={tabs} class="flow-tabs" aria-label="Flows"></nav>
  {/snippet}
  {#snippet overviewContent()}
    <OverviewPanel view={overview} refresh={refreshOverview} flows={overviewFlows} />
  {/snippet}
  {#snippet activityContent(visible: boolean)}
    {#if activity}
      {#key activity.key}
        <ExecutionPanel view={activity} refresh={refreshActivity} {issue} {retry} />
      {/key}
    {/if}
    {#if approvals}
      {#key approvals.key}
        <ApprovalActivity view={approvals} refresh={refreshApprovals} review={reviewApproval}
          dismiss={dismissApproval} issue={issueApproval} />
      {/key}
    {/if}
    {#if usage}
      {#key usage.key}<WorkUsage observation={usage} {visible} latest />{/key}
    {/if}
    {#if logs}
      {#each logs.targets as target (target.key)}
        <WorkLog {target} read={logs.read} {visible} />
      {/each}
    {/if}
  {/snippet}
  <FocusRail bind:this={focusRail} open={overviewOpen} compact={compactFocus} blocked={projectsDrawer}
    setOpen={setFocusPanelOpen} {overviewContent} {showOverview}
    activity={activity || approvals || usage || logs ? {
      capability: "activity.read",
      state: activity?.attention || approvals?.attention ? "busy" : activity?.error || approvals?.failed ? "unavailable" : "idle",
      content: activityContent,
    } : undefined} />
</div>

<style>
  .map-transcript-hidden { visibility: hidden; grid-area: 1 / 1; }
  .map-workspace:not([hidden]) { grid-area: 1 / 1; display: grid; min-block-size: 0; }
  .map-toggle[aria-pressed="true"] { color: var(--accent); }
</style>
