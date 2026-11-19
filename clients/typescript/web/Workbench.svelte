<script lang="ts">
  import { onMount, tick } from "svelte";
  import logo from "../../../static/ATMAN-LOGO.png";
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
  let overviewToggle: HTMLButtonElement | undefined = $state();
  let overview: HTMLElement | undefined = $state();
  let overviewOpen = $state(false);
  let projectsDrawer = $state(false);
  let overviewDrawer = $state(false);

  onMount(() => {
    const cleanup = onReady({
      shell, sidebarToggle, connectionState, sidebar, sessions, search, targetTitle,
      tabs, content, composer, status, disconnectButton, newSessionButton, refreshButton,
    });
    const projectsMedia = matchMedia("(width < 48rem)");
    const overviewMedia = matchMedia("(width < 64rem)");
    function syncDrawers() {
      projectsDrawer = projectsMedia.matches && shell.classList.contains("sidebar-open");
      overviewDrawer = overviewMedia.matches;
      if (projectsDrawer) overviewOpen = false;
    }
    const observer = new MutationObserver(syncDrawers);
    observer.observe(sidebarToggle, { attributeFilter: ["aria-expanded"] });
    const listeners = new AbortController();
    tabs.addEventListener("click", async (event) => {
      if (!overviewDrawer || !(event.target instanceof Element) || !event.target.closest("button.flow-tab")) return;
      await setOverview(false);
      await tick();
      content.focus({ preventScroll: true });
    }, { signal: listeners.signal });
    projectsMedia.addEventListener("change", syncDrawers);
    overviewMedia.addEventListener("change", syncDrawers);
    syncDrawers();
    return () => {
      observer.disconnect();
      listeners.abort();
      projectsMedia.removeEventListener("change", syncDrawers);
      overviewMedia.removeEventListener("change", syncDrawers);
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

  async function setOverview(open: boolean, returnFocus = false) {
    if (open && projectsOpen()) closeProjects();
    overviewOpen = open;
    if (open && overviewDrawer) {
      await tick();
      if (overviewOpen) overview?.querySelector<HTMLButtonElement>(".overview-close")?.focus();
    } else if (returnFocus) {
      overviewToggle?.focus();
    }
  }

  function handleEscape(event: KeyboardEvent) {
    if (event.key !== "Escape" || event.defaultPrevented || event.isComposing || document.querySelector("dialog[open]")) return;
    if (projectsOpen()) closeProjects();
    else if (overviewOpen) setOverview(false, true);
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

  <nav bind:this={sidebar} class="sidebar" id="session-sidebar" aria-label="Projects" inert={overviewOpen && overviewDrawer}>
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

  <main class="workspace" aria-labelledby="workbench-transcript" inert={projectsDrawer || (overviewOpen && overviewDrawer)}>
    <h2 class="visually-hidden" id="workbench-transcript">Transcript</h2>
    <!-- svelte-ignore a11y_no_noninteractive_tabindex (The independent scroll region must support keyboard scrolling.) -->
    <div bind:this={content} class="content" role="region" aria-labelledby="workbench-transcript" tabindex="0"></div>
    <div bind:this={status} class="status-line" role="status"></div>
    <div bind:this={composer} class="composer-slot"></div>
  </main>

  <button type="button" class="drawer-scrim projects-scrim" aria-label="Close Projects" tabindex="-1" onclick={closeProjects}></button>
  <button type="button" class="drawer-scrim overview-scrim" aria-label="Close Overview" tabindex="-1" onclick={() => setOverview(false, true)}></button>

  <aside bind:this={overview} class="overview" id="flow-overview" aria-labelledby="overview-title" inert={!overviewOpen}>
    <div class="overview-header">
      <h2 id="overview-title" class="sidebar-title">Overview</h2>
      <button type="button" class="icon overview-close" aria-label="Close Overview" title="Close Overview" onclick={() => setOverview(false, true)}>
        <svg viewBox="0 0 18 18" aria-hidden="true"><path d="m5 5 8 8m0-8-8 8"/></svg>
      </button>
    </div>
    <nav bind:this={tabs} class="flow-tabs" aria-label="Flows"></nav>
  </aside>
  <div class="focus-rail" role="group" aria-label="Focus rail">
    <button bind:this={overviewToggle} type="button" class="icon" aria-expanded={overviewOpen} aria-controls="flow-overview" aria-label="Overview" title="Overview" onclick={() => setOverview(!overviewOpen)}>
      <svg viewBox="0 0 18 18" aria-hidden="true"><path d="M6 4.5h8M6 9h8m-8 4.5h8m-10.5-9h.1M3.5 9h.1m-.1 4.5h.1"/></svg>
    </button>
  </div>
</div>
