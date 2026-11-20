<script lang="ts">
  import { onMount, tick } from "svelte";
  import logo from "../../../static/ATMAN-LOGO.png";
  import FocusRail from "./FocusRail.svelte";
  import ExecutionPanel from "./ExecutionPanel.svelte";
  import ApprovalActivity from "./ApprovalActivity.svelte";
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
  let newSessionButton!: HTMLButtonElement;
  let refreshButton!: HTMLButtonElement;
  let focusRail: FocusRail | undefined = $state();
  let activity = $state.raw<ActivityView | undefined>();
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

  onMount(() => {
    const cleanup = onReady({
      shell, sidebarToggle, connectionState, sidebar, sessions, search, targetTitle,
      tabs, content, composer, status, disconnectButton, newSessionButton, refreshButton,
      presentActivity: (view) => { activity = view; },
      bindActivityActions: (actions) => { activityActions = actions; },
      presentApprovals: (view) => { approvals = view; },
      bindApprovalActions: (actions) => { approvalActions = actions; },
    });
    const projectsMedia = matchMedia("(width < 48rem)");
    const focusMedia = matchMedia("(width < 64rem)");
    function syncLayout() {
      projectsDrawer = projectsMedia.matches && shell.classList.contains("sidebar-open");
      compactFocus = focusMedia.matches;
      if (projectsDrawer) overviewOpen = false;
    }
    const observer = new MutationObserver(syncLayout);
    observer.observe(sidebarToggle, { attributeFilter: ["aria-expanded"] });
    const listeners = new AbortController();
    async function returnToConversation(event: Event) {
      if (!compactFocus || !overviewOpen || !(event.target instanceof Element)) return;
      const action = event.target.closest("button[data-instance], button.flow-panel-launch, button.session-row, button[data-action='open-panel']");
      if (!action) return;
      await focusRail?.close();
      await tick();
      if (overviewOpen || projectsDrawer) return;
      const target = action.matches(".flow-panel-launch") ? shell.querySelector<HTMLElement>(".flow-window.active .flow-window-content") : content;
      target?.focus({ preventScroll: true });
    }
    tabs.addEventListener("click", returnToConversation, { signal: listeners.signal });
    sessions.addEventListener("click", returnToConversation, { signal: listeners.signal });
    projectsMedia.addEventListener("change", syncLayout);
    focusMedia.addEventListener("change", syncLayout);
    syncLayout();
    return () => {
      observer.disconnect();
      listeners.abort();
      projectsMedia.removeEventListener("change", syncLayout);
      focusMedia.removeEventListener("change", syncLayout);
      cleanup?.();
    };
  });

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
      <span bind:this={connectionState} class="connection-state">Disconnected</span>
      <button bind:this={disconnectButton} type="button" class="icon" aria-label="Disconnect" title="Disconnect">
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M8 3.5H3.5v11H8m1-5.5h6m-2.5-2.5L15 9l-2.5 2.5"/></svg>
      </button>
    </div>
  </header>

  <nav bind:this={sidebar} class="sidebar" id="session-sidebar" aria-label="Projects">
    <div class="sidebar-tools">
      <h2 class="sidebar-title">Projects</h2>
      <button bind:this={newSessionButton} type="button" class="icon" aria-label="Create Session" title="Create Session">
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M9 4v10M4 9h10"/></svg>
      </button>
      <button bind:this={refreshButton} type="button" class="icon" aria-label="Refresh Projects and Sessions" title="Refresh Projects and Sessions">
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M14.5 8a5.5 5.5 0 1 0-.75 4m.75-8v4h-4"/></svg>
      </button>
    </div>
    <input bind:this={search} class="session-search" type="search" aria-label="Find project or session" placeholder="Find…" />
    <div bind:this={sessions} class="session-list"></div>
  </nav>

  <main class="workspace" aria-labelledby="workbench-transcript" inert={projectsDrawer || (overviewOpen && compactFocus)}>
    <h2 class="visually-hidden" id="workbench-transcript">Transcript</h2>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (The independent scroll region must support keyboard scrolling.) -->
    <div bind:this={content} class="content" role="region" aria-labelledby="workbench-transcript" tabindex="0"></div>
    <div bind:this={status} class="status-line" role="status"></div>
    <div bind:this={composer} class="composer-slot"></div>
  </main>

  <button type="button" class="drawer-scrim projects-scrim" aria-label="Close Projects" tabindex="-1" onclick={closeProjects}></button>
  {#snippet overviewContent()}
    <nav bind:this={tabs} class="flow-tabs" aria-label="Flows"></nav>
  {/snippet}
  {#snippet activityContent()}
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
  {/snippet}
  <FocusRail bind:this={focusRail} open={overviewOpen} compact={compactFocus} blocked={projectsDrawer}
    setOpen={setFocusPanelOpen} {overviewContent}
    activity={activity || approvals ? {
      capability: "activity.read",
      state: activity?.attention || approvals?.attention ? "busy" : activity?.error || approvals?.failed ? "unavailable" : "idle",
      content: activityContent,
    } : undefined} />
</div>
