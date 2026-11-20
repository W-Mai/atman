import {
  AtmanClient,
  ClientError,
  HttpClient,
  canonicalUuid,
  contract,
} from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import { flushSync, mount, unmount } from "svelte";
import ConversationHistory from "./ConversationHistory.svelte";
import { HistoryController } from "./history-controller.ts";
import { showConversation } from "./conversation.ts";
import { FlowOverview } from "./flow-overview.ts";
import { ExecutionActivity } from "./execution-activity.ts";
import { ApprovalActivity } from "./approval-activity.ts";
import type { ApprovalView } from "./approval-activity.ts";
import { flowWindows } from "./flow-windows.ts";
import type { PendingInbox } from "./inbox.ts";
import { sharedConsole } from "./shared-console.ts";
import type { ConversationMemory, Draft } from "./conversation.ts";
import { SessionCatalog, initialFlow, mergeFlowPages } from "./session-catalog.ts";
import type { SessionEdition } from "./session-catalog.ts";
import type { WorkbenchElements } from "./shell-elements.ts";
import { PreferenceReceipts } from "./preference-editor.ts";
import { bindDraft, TabDrafts } from "./tab-drafts.ts";

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className = "",
  text = "",
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
}

function button(
  text: string,
  label: string,
  action: () => void,
  className = "",
): HTMLButtonElement {
  const node = element("button", className, text);
  node.type = "button";
  node.setAttribute("aria-label", label);
  node.title = label;
  node.addEventListener("click", action);
  return node;
}

function sidebarSessionRows<Row>(rows: readonly Row[], isOpen: (row: Row) => boolean): Row[] {
  const open: Row[] = [];
  const recent: Row[] = [];
  for (const row of rows) {
    if (isOpen(row)) open.push(row);
    else if (recent.length < 5) recent.push(row);
  }
  return [...open, ...recent];
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : "Request failed";
}

function tabStorage(): Storage | undefined {
  try {
    return sessionStorage;
  } catch {
    return undefined;
  }
}

export function bindWorkbench(elements: WorkbenchElements): () => void {
  const {
    shell,
    sidebarToggle,
    connectionState,
    sidebar,
    sessions,
    search,
    targetTitle,
    tabs,
    content,
    composer,
    status,
    disconnectButton,
    settingsButton,
    newSessionButton,
    refreshButton,
    presentOverview,
    presentFlowMap,
    bindOverviewActions,
    showConversationTarget,
    presentActivity,
    bindActivityActions,
    openApprovals,
    presentApprovals,
    bindApprovalActions,
  } = elements;

  const overview = new FlowOverview(presentOverview);
  bindOverviewActions({ refresh: () => overview.refresh(), visible: (value) => overview.visible(value) });
  const activity = new ExecutionActivity(presentActivity, tabStorage());
  bindActivityActions({
    refresh: () => activity.refresh(),
    issue: (view, action) => activity.issue(view, action),
    retry: (view) => activity.retry(view),
  });

  const input = sharedConsole(composer, openApprovals);
  let approvalView: ApprovalView | undefined;
  const approvalChanges = new EventTarget();
  const approvals = new ApprovalActivity((view) => {
    approvalView = view;
    presentApprovals(view);
    input.presentApprovals(view);
    approvalChanges.dispatchEvent(new Event("change"));
  });
  bindApprovalActions({
    refresh: () => approvals.refresh(),
    review: (view, selection) => approvals.review(view, selection),
    dismiss: (view) => approvals.dismiss(view),
    issue: (view, action) => approvals.issue(view, action),
  });

  composer.hidden = true;

  let api: AtmanClient | undefined;
  let connectionRevision = 0;
  let revision = 0;
  let listingRead: AbortController | undefined;
  let projectRead: AbortController | undefined;
  let sidebarOpen = false;
  let desktopSidebarOpen = true;
  const narrowProjects = matchMedia("(width < 48rem)");
  let requestedSession: string | undefined;
  let requestedLegacy = false;
  let selected: Dtos["SessionView"] | undefined;
  let selectedEdition: SessionEdition | undefined;
  let flowCatalog: SessionCatalog | undefined;
  let connectionRead: AbortController | undefined;
  let activeFlow: FlowClient | undefined;
  let openingSession = false;
  const loadingFlowPages = new WeakSet<SessionEdition>();
  const views = new Map<string, ConversationMemory>();
  let conversationRead: AbortController | undefined;
  let conversationSupported = false;
  let overviewSupported = false;
  let overviewSession: string | undefined;
  let overviewEdition: SessionEdition | undefined;
  let overviewCurrentFlow: string | undefined;
  let renderedOverviewFlow: string | undefined;
  let overviewDirectoryRead: AbortController | undefined;
  let overviewDirectoryGeneration = 0;
  let activitySupported = false;
  let approvalsSupported = false;
  let draftsSupported = false;
  let historySupported = false;
  let inbox: { pending: Map<string, PendingInbox>; edits: boolean } | undefined;
  const activityTargets = new Map<AbortController, { flow: FlowClient; label: string; view?: Dtos["SessionView"] }>();

  function syncActivity(): void {
    const read = activeWindow === undefined ? conversationRead : windowReads.get(activeWindow);
    const target = !openingSession && read !== undefined ? activityTargets.get(read) : undefined;
    const pending = api !== undefined && (openingSession || (read !== undefined && target === undefined));
    tabs.inert = pending;
    if (!pending) {
      overview.select(overviewSupported ? target : undefined);
      syncOverviewDirectory((activeWindow === undefined ? selected : target?.view),
        target?.flow.address.instance_id);
    }
    activity.select(activitySupported ? target : undefined);
    approvals.select(approvalsSupported ? target : undefined);
  }
  const windowReads = new Map<string, AbortController>();
  const settingsFocus = new EventTarget();
  let activeWindow: string | undefined;
  let panelRequest: { session: string; selection: number; error: string | undefined } | undefined;
  let panelSelection = 0;
  const windows = flowWindows(content.parentElement!, content, status, composer, (key) => {
    panelSelection++;
    activeWindow = key;
    input.activate(key === undefined ? conversationRead : windowReads.get(key));
    settingsFocus.dispatchEvent(new Event("change"));
    history.focusChanged();
    syncActivity();
  });
  const drafts = new Map<string, Draft>();
  const draftArchive = new TabDrafts(tabStorage());
  let draftScope: string | undefined;
  let preferenceScope: string | undefined;
  const preferenceArchive = new PreferenceReceipts(tabStorage());
  let originalCredential: string | undefined;
  const lifetime = new AbortController();
  const historyDialog = element("dialog", "history-dialog");
  historyDialog.setAttribute("aria-labelledby", "history-title");
  document.body.append(historyDialog);
  let historyPanel: ReturnType<typeof historyPresenter> | undefined;
  function historyPresenter() {
    return mount(ConversationHistory, { target: historyDialog, props: {
      dispatch: (action) => history.dispatch(action), hide: () => history.hide(),
    } });
  }
  const history = new HistoryController((view, restore) => {
    if (view) {
      if (!historyPanel) historyPanel = historyPresenter();
      flushSync(() => historyPanel?.update(view));
      if (!historyDialog.open) historyDialog.showModal();
    } else {
      if (historyDialog.open) historyDialog.close();
      if (restore?.isConnected && !restore.closest("[inert]")) restore.focus({ preventScroll: true });
    }
  }, tabStorage());
  historyDialog.addEventListener("cancel", (event) => {
    event.preventDefault();
    history.hide();
  }, { signal: lifetime.signal });
  type SessionRow = Dtos["ListSessionsResponse"][number];
  type OpenSession = Pick<Dtos["SessionView"], "session_id" | "project_root" | "title">;
  const mainSessions = new Map<string, OpenSession>();
  const panelSessions = new Map<string, OpenSession>();

  function openedSessions(): Map<string, OpenSession> {
    const opened = new Map<string, OpenSession>();
    for (const view of panelSessions.values()) opened.set(view.session_id, view);
    for (const view of mainSessions.values()) opened.set(view.session_id, view);
    return opened;
  }

  type ProjectPage = Dtos["ProjectListPage"];
  type ProjectGroup = { root: string; label: string; pinned: boolean; record?: Dtos["ProjectListEntry"]; sessions: SessionRow[] };
  let sessionRows: readonly SessionRow[] | undefined;
  let projectPage: ProjectPage | undefined;
  let projectsSupported = false;
  let projectPinSupported = false;
  let pinWrite: { fingerprint: string } | undefined;
  let pinError: { fingerprint: string; message: string } | undefined;
  let projectError: string | undefined;
  let catalogError: string | undefined;
  const sidebarProjects = new Map<string, boolean>();
  const workspaceProjects = new Map<string, boolean>();

  const browserKeys = {
    token: "atman.web.token",
    session: "atman.web.session",
    flows: "atman.web.flows",
    draftScope: "atman.web.draft-scope",
  };

  function saved(key: keyof typeof browserKeys): string | undefined {
    try {
      return sessionStorage.getItem(browserKeys[key]) ?? undefined;
    } catch {
      return undefined;
    }
  }

  function remember(key: keyof typeof browserKeys, value?: string): void {
    try {
      if (value === undefined) sessionStorage.removeItem(browserKeys[key]);
      else sessionStorage.setItem(browserKeys[key], value);
    } catch {
      // Browsing with storage disabled still supports a live connection.
    }
  }

  function rememberedFlows(): Record<string, string> {
    const raw = saved("flows");
    if (!raw || raw.length > 4096) return {};
    try {
      const entries: unknown = JSON.parse(raw);
      if (entries === null || typeof entries !== "object" || Array.isArray(entries)) return {};
      return Object.fromEntries(Object.entries(entries).slice(-32).map(([session, flow]) =>
        [canonicalUuid(session), canonicalUuid(flow as string)]));
    } catch {
      return {};
    }
  }

  function rememberFlow(flow: FlowClient): void {
    const { session_id, instance_id } = flow.address;
    const known = rememberedFlows();
    delete known[session_id];
    const entries = [...Object.entries(known), [session_id, instance_id]].slice(-32);
    remember("flows", JSON.stringify(Object.fromEntries(entries)));
  }

  function draftFor(flow: FlowClient, page: Dtos["ConversationPage"]): Draft {
    const binding = draftScope && bindDraft(draftScope, flow.address, page);
    const key = binding
      ? `${binding.scope}/${binding.flow.session_id}/${binding.flow.instance_id}/${binding.journal}/${binding.branch}`
      : `${flow.address.session_id}/${flow.address.instance_id}/${page.head.branch}`;
    const existing = drafts.get(key);
    if (existing) return existing;
    const restored = binding ? draftArchive.get(binding) : undefined;
    const draft: Draft = {
      text: restored?.text ?? "",
      edit: restored?.edit ?? 0,
      ...(restored?.attachments === undefined ? {} : { attachments: restored.attachments }),
      ...(restored?.pending === undefined ? {} : { pending: restored.pending }),
      changes: new EventTarget(),
    };
    if (binding) {
      draft.persist = () => {
        return draftArchive.save(binding, {
          text: draft.text,
          edit: draft.edit,
          ...(draft.attachments === undefined ? {} : { attachments: draft.attachments }),
          ...(draft.pending === undefined ? {} : { pending: draft.pending }),
        });
      };
    }
    drafts.set(key, draft);
    return draft;
  }

  function setOpening(opening: boolean, sync = true): void {
    const wasOpening = openingSession;
    openingSession = opening;
    if (wasOpening && !opening) settingsFocus.dispatchEvent(new Event("change"));
    history.focusChanged();
    if (sync) syncActivity();
    composer.inert = opening;
    status.classList.toggle("in-place", opening && activeFlow !== undefined);
  }

  function setSidebar(open: boolean): void {
    if (sidebarOpen !== open) panelSelection++;
    sidebarOpen = open;
    shell.classList.toggle("sidebar-open", open);
    updateSidebarToggle();
  }

  function updateSidebarToggle(): void {
    sidebarToggle.setAttribute("aria-expanded", String(narrowProjects.matches ? sidebarOpen : desktopSidebarOpen));
  }

  function setDesktopSidebar(open: boolean): void {
    if (desktopSidebarOpen !== open) panelSelection++;
    desktopSidebarOpen = open;
    shell.classList.toggle("projects-collapsed", !open);
    updateSidebarToggle();
  }

  function focusProjectSearch(): void {
    if (narrowProjects.matches) setSidebar(true);
    else setDesktopSidebar(true);
    requestAnimationFrame(() => {
      if (!lifetime.signal.aborted) search.focus({ preventScroll: true });
    });
  }

  search.addEventListener("input", () => {
    renderSessions();
    renderWorkspaceLanding();
  }, { signal: lifetime.signal });

  function setStatus(text: string, loading = false, failed = false): void {
    status.classList.toggle("error", failed);
    status.replaceChildren(
      ...(loading ? [element("span", "loading-mark")] : []),
      document.createTextNode(text),
    );
  }

  function empty(title: string, copy: string, ...buttons: HTMLButtonElement[]): HTMLElement {
    const block = element("section", "empty");
    block.append(element("h2", "", title));
    if (copy) block.append(element("p", "muted", copy));
    if (buttons.length > 0) {
      const actions = element("div", "empty-actions");
      actions.append(...buttons);
      block.append(actions);
    }
    content.replaceChildren(block);
    return block;
  }

  function closeConversation(sync = true): void {
    conversationRead?.abort();
    conversationRead = undefined;
    activeFlow = undefined;
    if (activeWindow === undefined) input.activate();
    if (sync) syncActivity();
  }

  function isCurrentSession(row: SessionRow): boolean {
    return (
      row.persistent == null ? requestedLegacy && row.id === requestedSession : row.id === selected?.session_id
    );
  }

  function sessionRow(row: SessionRow, allowPanel: boolean, opened: boolean): HTMLElement {
    const label = row.title || row.id.slice(0, 8);
    const node = button("", label, () => {
      setSidebar(false);
      sidebarProjects.set(row.project_root ?? "", true);
      if (row.persistent == null) {
        showLegacySession(row.id, label);
      } else {
        void selectSession(row.id);
      }
      showConversationTarget(content);
    }, "session-row");
    node.setAttribute("aria-current", String(isCurrentSession(row)));
    node.dataset.session = `${row.id}/${row.persistent == null ? "legacy" : "native"}`;
    const copy = element("span", "session-title", label);
    if (row.persistent == null) {
      copy.append(element("span", "session-meta", "Legacy"));
    } else if (row.status !== "idle") {
      copy.append(element("span", "session-meta", row.status));
    }
    node.append(element("span", "session-marker"), copy);
    node.classList.toggle("session-open", opened);
    if (!allowPanel || row.persistent == null) return node;
    const canOpenPanel = conversationSupported &&
      !(activeFlow?.address.session_id === row.id && selected?.default_flow === activeFlow.address.instance_id);
    if (!opened && !canOpenPanel) return node;
    const item = element("div", "project-session-actions");
    const actions = element("div", "session-actions");
    const pending = panelRequest?.session === row.id && panelRequest.selection === panelSelection ? panelRequest : undefined;
    if (canOpenPanel) {
      const panel = button("▣", `Open ${label} in panel`, () => void openSessionPanel(row.id, label), "session-panel-launch");
      panel.dataset.session = node.dataset.session;
      panel.dataset.action = "open-panel";
      if (pending && pending.error === undefined) panel.setAttribute("aria-busy", "true");
      actions.append(panel);
    }
    if (opened) {
      const close = button("×", `Close ${label} (execution continues)`, () => closeSession(row.id), "session-panel-launch");
      close.dataset.session = node.dataset.session;
      close.dataset.action = "close-session";
      actions.append(close);
    }
    item.append(node, actions);
    if (pending?.error !== undefined) {
      const error = element("p", "session-panel-feedback error", pending.error);
      error.setAttribute("role", "status");
      item.append(error);
    }
    return item;
  }

  function groupProjects(opened?: ReadonlyMap<string, OpenSession>): ProjectGroup[] {
    const query = search.value.trim().toLowerCase();
    const groups = new Map<string, ProjectGroup>();
    for (const project of projectPage?.projects ?? []) {
      groups.set(project.project_root, {
        root: project.project_root,
        label: project.display_name,
        pinned: project.pinned,
        record: project,
        sessions: [],
      });
    }
    // A freshly opened Session may arrive before its directory row.
    const missing = new Map(opened);
    const rows = [...(sessionRows ?? [])];
    for (const row of rows) if (row.persistent != null) missing.delete(row.id);
    for (const view of missing.values()) {
      rows.push({ id: view.session_id, persistent: {}, status: "idle", project_root: view.project_root ?? null,
        ...(view.title == null ? {} : { title: view.title }) });
    }
    for (const row of rows) {
      const root = row.project_root ?? "";
      let project = groups.get(root);
      if (project === undefined) {
        const label =
          root.split(/[/\\]/).filter(Boolean).at(-1) ?? (root || "No project");
        project = { root, label, pinned: false, sessions: [] };
        groups.set(root, project);
      }
      project.sessions.push(row);
    }
    const hasOpen = (project: ProjectGroup) => project.sessions.some((row) => row.persistent != null && opened?.has(row.id));
    const openRoots = new Set([...groups.values()].filter(hasOpen).map((project) => project.root));
    const visible: ProjectGroup[] = [];
    for (const project of groups.values()) {
      if (!query || [project.label, project.root].some((value) => value.toLowerCase().includes(query))) {
        visible.push(project);
        continue;
      }
      const matching = project.sessions.filter((row) =>
        row.title?.toLowerCase().includes(query) || row.id.toLowerCase().includes(query));
      if (matching.length) visible.push({ ...project, sessions: matching });
    }
    return visible.sort(
      (left, right) =>
        Number(openRoots.has(right.root)) - Number(openRoots.has(left.root)) ||
        Number(right.pinned) - Number(left.pinned) ||
        Number(!left.root) - Number(!right.root) ||
        left.label.localeCompare(right.label) ||
        left.root.localeCompare(right.root),
    );
  }

  function projectTree(
    projects: ProjectGroup[],
    expanded: Map<string, boolean>,
    searching: boolean,
    opened?: ReadonlyMap<string, OpenSession>,
  ): HTMLElement {
    const tree = element("div", "project-tree");
    const labels = new Map<string, number>();
    for (const { label } of projects) labels.set(label, (labels.get(label) ?? 0) + 1);
    for (const project of projects) {
      const group = element("details", "project-group");
      const summary = element("summary", "project-summary");
      summary.title = project.root || project.label;
      summary.dataset.project = project.root;
      const marker = element("span", "project-chevron");
      marker.setAttribute("aria-hidden", "true");
      const name = element("span", "project-name", project.label);
      if ((labels.get(project.label) ?? 0) > 1 && project.root)
        name.append(element("span", "session-meta", project.root));
      summary.append(
        marker,
        name,
        element("span", "project-count", String(project.sessions.length)),
      );
      const list = element("div", "project-rows");
      const current = project.sessions.some(isCurrentSession);
      summary.classList.toggle("contains-current", current);
      if (!searching && current && !expanded.has(project.root)) expanded.set(project.root, true);
      group.open = searching || (expanded.get(project.root) ?? (current || projects.length === 1));
      const showRows = () => {
        if (list.hasChildNodes()) return;
        const isOpen = (row: SessionRow) => row.persistent != null && opened?.has(row.id) === true;
        const visible = opened === undefined || searching ? project.sessions : sidebarSessionRows(project.sessions, isOpen);
        if (visible.length) list.append(...visible.map((row) => sessionRow(row, opened !== undefined, isOpen(row))));
        else if (sessionRows !== undefined) list.append(element("p", "sidebar-message", "No Sessions"));
      };
      if (group.open) showRows();
      summary.addEventListener("click", () => {
        if (!searching) expanded.set(project.root, !group.open);
        if (!group.open) showRows();
      });
      group.append(summary, list);
      if (project.record && opened !== undefined) {
        const record = project.record;
        const actions = element("div", "project-actions");
        const create = button("+", `New Session in ${project.label}`, () => openCreateSession(project.root));
        create.dataset.project = project.root;
        create.dataset.action = "create";
        actions.append(create);
        if (projectPinSupported) {
          const pin = button(record.pinned ? "◆" : "◇", `Pin ${project.label}`,
            () => void setProjectPinned(record, !record.pinned));
          pin.title = `${record.pinned ? "Unpin" : "Pin"} ${project.label}`;
          pin.dataset.project = project.root;
          pin.dataset.action = "pin";
          pin.setAttribute("aria-pressed", String(record.pinned));
          pin.setAttribute("aria-disabled", String(pinWrite !== undefined));
          if (pinWrite?.fingerprint === record.fingerprint) pin.setAttribute("aria-busy", "true");
          actions.prepend(pin);
        }
        summary.classList.toggle("with-project-actions", true);
        group.classList.toggle("with-project-pin", projectPinSupported);
        const stack = element("div", "project-entry");
        stack.append(group, actions);
        if (pinError?.fingerprint === record.fingerprint) {
          const feedback = element("p", "sidebar-message error", pinError.message);
          feedback.setAttribute("role", "status");
          stack.append(feedback);
        }
        tree.append(stack);
        continue;
      }
      tree.append(group);
    }
    return tree;
  }

  function directoryControls(): HTMLElement[] {
    if (projectError !== undefined) {
      return [element("p", "sidebar-message error", projectError),
        button("Retry Projects", "Reload the Project directory", () => void loadProjects(), "directory-more")];
    }
    if (projectPage?.after == null) return [];
    const more = button("…", "Load more Projects", () => void loadProjects(true), "directory-more");
    more.disabled = projectRead !== undefined || pinWrite !== undefined;
    return [more];
  }

  function renderSessions(): void {
    const focused = document.activeElement as HTMLElement | null;
    const focus = sessions.contains(focused) ? focused?.dataset : undefined;
    const onMore = sessions.contains(focused) && focused?.matches(".directory-more");
    sessions.replaceChildren();
    const opened = openedSessions();
    const projects = groupProjects(opened);
    const searching = Boolean(search.value.trim());
    if (projects.length) {
      sessions.append(
        projectTree(projects, sidebarProjects, searching, opened),
      );
    }
    sessions.append(...directoryControls());
    if (catalogError !== undefined) {
      sessions.append(element("p", "sidebar-message error", catalogError));
    } else if (projects.length === 0 && sessionRows !== undefined && !projectRead &&
        projectError === undefined && projectPage?.after == null) {
      sessions.append(
        element(
          "p",
          "sidebar-message",
          searching ? "No matches" : "No Projects",
        ),
      );
    }
    const focusedSession = [...sessions.querySelectorAll<HTMLElement>("[data-session]")].filter((row) => row.dataset.session === focus?.session);
    const target = focus?.session !== undefined
      ? focusedSession.find((row) => row.dataset.action === focus.action) ??
        focusedSession.find((row) => row.dataset.action === undefined) ?? search
      : focus?.project !== undefined
        ? [...sessions.querySelectorAll<HTMLElement>("[data-project]")].find((row) =>
            row.dataset.project === focus.project && row.dataset.action === focus.action) ?? search
        : onMore ? sessions.querySelector<HTMLElement>(".directory-more") ?? refreshButton : undefined;
    target?.focus({ preventScroll: true });
  }

  function renderWorkspaceLanding(): void {
    if (requestedSession !== undefined) return;

    const create = button(
      "New Session",
      "New Session",
      () => openCreateSession(),
      "primary",
    );
    const projects = groupProjects();
    const searching = Boolean(search.value.trim());
    const directory = directoryControls();
    if (catalogError !== undefined && !projects.length) {
      empty(
        "Couldn’t load Sessions",
        catalogError,
        button("Retry", "Retry loading Sessions", () => void loadSessions()),
        create,
      );
    } else if (projects.length || directory.length) {
      const landing = empty(
        searching ? "Search results" : "Projects",
        "",
        button("Find Session", "Focus project and session search", focusProjectSearch),
        button("New Session", "New Session", () => openCreateSession()),
      );
      landing.classList.add("project-directory");
      landing.append(
        projectTree(projects, workspaceProjects, searching),
        ...directory,
      );
    } else if (sessionRows === undefined || projectRead !== undefined) {
      empty("Loading Projects…", "");
    } else if (searching) {
      empty(
        "No matching Sessions",
        "Try a different search or start a new Session.",
        create,
        button("Clear search", "Clear Session search", () => {
          search.value = "";
          renderSessions();
          renderWorkspaceLanding();
        }),
      );
    } else {
      empty("No Sessions yet", "Create a Session to start a conversation.", create);
    }
  }

  async function loadSessions(): Promise<void> {
    const source = api;
    if (source === undefined) return;
    listingRead?.abort();
    const read = new AbortController();
    listingRead = read;
    catalogError = undefined;
    sidebar.setAttribute("aria-busy", "true");
    renderWorkspaceLanding();
    try {
      const rows = await source.listSessions({}, { signal: read.signal });
      if (api === source && listingRead === read) sessionRows = rows;
    } catch (error) {
      if (api === source && listingRead === read) catalogError = message(error);
    } finally {
      if (listingRead === read) {
        listingRead = undefined;
        sidebar.removeAttribute("aria-busy");
        renderSessions();
        renderWorkspaceLanding();
      }
    }
  }

  async function loadProjects(more = false): Promise<void> {
    const source = api;
    if (!source || !projectsSupported || (more && (projectRead || pinWrite || projectPage?.after == null))) return;
    projectRead?.abort();
    const read = new AbortController();
    const original = projectPage;
    const retainRows = original?.projects.length ?? 0;
    const after = more ? original?.after : undefined;
    projectRead = read;
    projectError = undefined;
    try {
      const request = { signal: read.signal };
      let page = await source.listProjects(after == null ? {} : { after }, request);
      const projects = after != null && original ? [...original.projects, ...page.projects] : [...page.projects];
      while (projects.length < retainRows && page.after != null && api === source && projectRead === read) {
        page = await source.listProjects({ after: page.after }, request);
        projects.push(...page.projects);
      }
      if (api !== source || projectRead !== read) return;
      projectPage = { ...page, projects };
    } catch (error) {
      if (api === source && projectRead === read) projectError = message(error);
    } finally {
      if (projectRead === read) {
        projectRead = undefined;
        renderSessions();
        renderWorkspaceLanding();
      }
    }
  }

  async function setProjectPinned(record: Dtos["ProjectListEntry"], pinned: boolean): Promise<void> {
    const source = api;
    const page = projectPage;
    if (!source || !projectPinSupported || !page || pinWrite || !page.projects.includes(record)) return;
    const write = { fingerprint: record.fingerprint };
    pinWrite = write;
    pinError = undefined;
    renderSessions();
    try {
      const receipt = await source.setProjectPinned({ fingerprint: record.fingerprint,
        expected_revision: page.revision, pinned });
      if (api !== source || pinWrite !== write) return;
      // Only replace the observed row. A fresh listing owns paging and any unrelated row changes.
      if (projectPage?.revision === page.revision) {
        projectPage = { ...projectPage,
          projects: projectPage.projects.map((row) => row.fingerprint === record.fingerprint ? receipt.project : row) };
      }
    } catch (error) {
      if (api !== source || pinWrite !== write) return;
      pinError = { fingerprint: record.fingerprint, message: message(error) };
    }
    if (api !== source || pinWrite !== write) return;
    renderSessions();
    renderWorkspaceLanding();
    await loadProjects();
    if (api === source && pinWrite === write) {
      pinWrite = undefined;
      renderSessions();
    }
  }

  function refreshDirectory(): void {
    void loadProjects();
    void loadSessions();
  }

  function showLegacySession(id: string, label: string): void {
    revision++;
    setOpening(false, false);
    requestedSession = id;
    requestedLegacy = true;
    selected = undefined;
    selectedEdition = undefined;
    closeConversation(false);
    windows.main();
    syncActivity();
    targetTitle.textContent = label;
    setStatus("");
    renderSessions();
    const block = empty("Open this Session in the terminal", "Resume it in atman to migrate its history.");
    block.append(element("code", "", `atman --continue ${id}`));
  }

  function syncOverviewDirectory(view: Dtos["SessionView"] | undefined, currentId?: string): void {
    const source = api;
    const directory = flowCatalog;
    const id = view?.session_id;
    const same = overviewSession === id;
    overviewCurrentFlow = currentId;
    if (!same) {
      overviewDirectoryGeneration++;
      overviewDirectoryRead?.abort();
      overviewDirectoryRead = undefined;
      overviewSession = id;
      overviewEdition = undefined;
      tabs.replaceChildren();
      presentFlowMap(undefined);
    }
    if (!source || !directory || !view) return;
    if (selectedEdition && id === selectedEdition.view.session_id) {
      overviewDirectoryRead?.abort();
      overviewDirectoryRead = undefined;
      renderFlows(source, selectedEdition);
      return;
    }
    const cached = overviewEdition ?? directory.cached(view.session_id);
    if (cached) renderFlows(source, cached);
    if (same) return;
    const read = new AbortController();
    overviewDirectoryRead = read;
    const signal = AbortSignal.any([connectionRead?.signal ?? lifetime.signal, read.signal]);
    const current = () => api === source && overviewDirectoryRead === read && !signal.aborted;
    void directory.readFlows(view, signal).then((edition) => {
      if (!current()) return;
      directory.store(edition);
      renderFlows(source, edition);
    }, () => {
      if (current() && !overviewEdition) {
        tabs.replaceChildren(button("Retry", "Retry Flow directory", () => {
          if (tabs.inert) return;
          overviewSession = undefined;
          syncOverviewDirectory(view, overviewCurrentFlow);
        }, "flow-tab"));
      }
    }).finally(() => {
      if (overviewDirectoryRead === read) overviewDirectoryRead = undefined;
    });
  }

  function renderFlows(source: AtmanClient, edition: SessionEdition): void {
    const { view, flows } = edition;
    if (overviewSession !== view.session_id) return;
    const target = source.session(view.session_id);
    const currentId = overviewCurrentFlow;
    if (overviewEdition === edition && renderedOverviewFlow === currentId) return;
    const previous = overviewEdition;
    overviewEdition = edition;
    renderedOverviewFlow = currentId;
    const focused = tabs.contains(document.activeElement)
      ? (document.activeElement as HTMLElement).dataset.flowAction
      : undefined;
    const selectDirectoryFlow = (id: string, label: string) => {
      if (tabs.inert || api !== source || overviewEdition !== edition) return;
      const flow = target.flow(id);
      if (view.session_id === selected?.session_id && activeWindow !== flowKey(flow)) {
        selectFlow(flow);
        showConversationTarget(content);
      } else {
        openFlowPanel(flow, label, true, view);
      }
    };
    presentFlowMap({
      entries: flows.instances.map(row => ({ address: row.flow, title: row.entry || row.flow.instance_id.slice(0, 8),
        ...(row.map ? { phase: row.map.phase, paused: row.map.paused,
          ...(row.map.parent ? { parent: row.map.parent } : {}) } : {}),
        archived: row.archived, unavailable: row.unavailable != null || !conversationSupported })),
      session: view.session_id,
      ...(currentId ? { selected: target.flow(currentId).address } : {}),
      hasRelations: flows.instances.some(row => row.map != null),
      refresh: () => { void refreshFlowMap(source, edition); },
      ...(flows.next == null ? {} : { more: () => loadFlows(source, edition) }),
      onOpen: (address) => {
        if (overviewEdition !== edition || address.session_id !== view.session_id) return;
        const row = flows.instances.find(row => row.flow.instance_id === address.instance_id);
        if (row && row.unavailable == null) selectDirectoryFlow(address.instance_id, row.entry || address.instance_id.slice(0, 8));
      },
    });
    const flowButton = (id: string, label: string) => {
      const flow = target.flow(id);
      const node = button(label, label, () => selectDirectoryFlow(id, label), "flow-tab");
      node.dataset.instance = id;
      node.dataset.flowAction = `select:${id}`;
      node.setAttribute("aria-pressed", String(currentId === id));
      if (!conversationSupported || view.session_id !== selected?.session_id || activeFlow?.address.instance_id === id || currentId === id) return node;
      const row = element("span", "flow-row");
      const panel = button("▣", `Open ${label} in panel`, () => {
        if (!tabs.inert) openFlowPanel(flow, label, true, view);
      }, "flow-panel-launch");
      panel.dataset.flowAction = `panel:${id}`;
      row.append(node, panel);
      return row;
    };
    const buttons = flows.instances.map((row) => {
      const id = row.flow.instance_id;
      return flowButton(id, row.entry || id.slice(0, 8));
    });
    for (const id of new Set([view.default_flow, currentId])) {
      if (id != null && !flows.instances.some((row) => row.flow.instance_id === id)) {
        buttons.unshift(flowButton(id, id === view.default_flow ? "Default Flow" : id.slice(0, 8)));
      }
    }
    if (flows.next != null) {
      const more = button("…", "Load more Flows", () => void loadFlows(source, edition), "flow-tab flow-more");
      more.dataset.flowAction = "more";
      buttons.push(more);
    }
    tabs.replaceChildren(...buttons);
    if (focused !== undefined) {
      const actions = [...tabs.querySelectorAll<HTMLButtonElement>("button[data-flow-action]")];
      const added = flows.instances.find(({ flow }) => !previous?.flows.instances.some((row) => row.flow.instance_id === flow.instance_id));
      const fallback = focused.startsWith("panel:") ? `select:${focused.slice(6)}`
        : focused === "more" && added ? `select:${added.flow.instance_id}` : undefined;
      (actions.find((node) => node.dataset.flowAction === focused) ??
        actions.find((node) => node.dataset.flowAction === fallback) ?? actions.at(-1))?.focus({ preventScroll: true });
    }
  }

  async function refreshFlowMap(source: AtmanClient, edition: SessionEdition): Promise<void> {
    const signal = connectionRead?.signal;
    const directory = flowCatalog;
    if (!signal || !directory || api !== source || overviewEdition !== edition || tabs.inert ||
      overviewDirectoryRead || loadingFlowPages.has(edition)) return;
    const read = new AbortController();
    overviewDirectoryRead = read;
    try {
      const next = await directory.readFlows(edition.view, AbortSignal.any([signal, read.signal]));
      if (signal.aborted || read.signal.aborted || overviewDirectoryRead !== read ||
        api !== source || overviewEdition !== edition || tabs.inert || loadingFlowPages.has(edition)) return;
      if (selectedEdition?.view.session_id === next.view.session_id) selectedEdition = next;
      directory.store(next);
      renderFlows(source, next);
    } catch {
      // Keep the installed page available when this optional refresh fails.
    } finally {
      if (overviewDirectoryRead === read) overviewDirectoryRead = undefined;
    }
  }

  async function loadFlows(source: AtmanClient, edition: SessionEdition): Promise<void> {
    const { view, flows } = edition;
    const before = flows.next;
    if (before == null || loadingFlowPages.has(edition) || overviewEdition !== edition || tabs.inert) return;
    const signal = connectionRead?.signal;
    if (!signal || signal.aborted) return;
    const generation = overviewDirectoryGeneration;
    const current = () => api === source && overviewEdition === edition &&
      overviewDirectoryGeneration === generation && !tabs.inert;
    loadingFlowPages.add(edition);
    const more = tabs.querySelector<HTMLButtonElement>(".flow-more");
    more?.setAttribute("aria-disabled", "true");
    try {
      const { result: page } = await source.session(view.session_id).flows({ before, limit: 50 }, { signal });
      if (!current()) return;
      const merged = mergeFlowPages(flows, page);
      const next = merged ? { view, flows: merged } : await flowCatalog?.readFlows(view, signal);
      if (!next || !current()) return;
      if (selectedEdition?.view.session_id === view.session_id) selectedEdition = next;
      flowCatalog?.store(next);
      renderFlows(source, next);
    } catch (error) {
      if (current()) setStatus(message(error), false, true);
    } finally {
      loadingFlowPages.delete(edition);
      more?.removeAttribute("aria-disabled");
    }
  }

  function installSession(source: AtmanClient, edition: SessionEdition): HTMLButtonElement | undefined {
    const { view, flows } = edition;
    const id = view.session_id;
    const sameView = selected?.session_id === id && activeFlow !== undefined;
    selected = view;
    selectedEdition = edition;
    if (!mainSessions.has(id)) sidebarProjects.set(view.project_root ?? "", true);
    mainSessions.delete(id);
    mainSessions.set(id, view);
    targetTitle.textContent = view.title || id.slice(0, 8);
    remember("session", id);
    setOpening(false, false);
    setStatus("");

    const remembered = rememberedFlows()[id];
    const target = initialFlow(view, remembered);
    if (!sameView) {
      if (target !== undefined) selectFlow(source.session(id).flow(target));
      else {
        closeConversation();
        if (flows.total === 0) {
          const start = button("Start conversation", "Start conversation", () => void bootstrap(start), "primary");
          empty("Start here", "", start);
          renderSessions();
          return start;
        }
        empty("Select a Flow", "Open Overview to choose a Flow.");
      }
    }
    syncActivity();
    renderSessions();
  }

  async function selectSession(id: string, prepare = false): Promise<void> {
    const source = api;
    const directory = flowCatalog;
    if (!source || !directory) return;
    const request = ++revision;
    const displayed = selected;
    const cached = prepare ? undefined : directory.cached(id);
    requestedSession = id;
    requestedLegacy = false;
    setOpening(true);
    windows.main();
    renderSessions();
    if (cached) {
      installSession(source, cached);
    } else {
      if (!activeFlow) empty("Opening Session", "");
      setStatus("Opening Session", true);
    }
    try {
      const edition = await directory.refresh(id);
      if (api !== source || request !== revision) return;
      const start = installSession(source, edition);
      if (prepare && start !== undefined) await bootstrap(start);
    } catch (error) {
      if (api !== source || request !== revision) return;
      setOpening(false);
      const retry = button("Retry", "Retry opening Session", () => void selectSession(id), "primary");
      if (activeFlow) {
        requestedSession = selected?.session_id ?? displayed?.session_id;
        renderSessions();
        setStatus(message(error), false, true);
        status.append(" ", retry);
      } else {
        setStatus("");
        empty("Couldn’t open Session", message(error), retry);
      }
    }
  }

  function selectFlow(flow: FlowClient): void {
    if (openingSession) return;
    panelSelection++;
    if (flow.address.session_id === activeFlow?.address.session_id &&
        flow.address.instance_id === activeFlow.address.instance_id) {
      windows.main();
      return;
    }
    closeConversation(false);
    if (!conversationSupported) {
      windows.main();
      syncActivity();
      empty("Flow selected", "This host does not provide browser conversation reads.");
      return;
    }
    const read = new AbortController();
    conversationRead = read;
    activeFlow = flow;
    windows.main();
    windows.close(flowKey(flow));
    input.activate(read);
    syncActivity();
    installConversation(flow, content, read, (text, loading, failed) => {
      if (!openingSession && conversationRead === read) setStatus(text, loading, failed);
    }, selectedEdition?.flows.instances.find((row) => row.flow.instance_id === flow.address.instance_id)?.entry ||
      (selected?.default_flow === flow.address.instance_id ? "Default Flow" : flow.address.instance_id.slice(0, 8)),
    () => rememberFlow(flow), selected);
    renderSessions();
  }

  function flowKey(flow: FlowClient): string {
    return flow.address.session_id + "/" + flow.address.instance_id;
  }

  async function openSessionPanel(id: string, label: string): Promise<void> {
    const source = api;
    const directory = flowCatalog;
    if (!source || !directory || openingSession) return;
    if (panelRequest?.session === id && panelRequest.error === undefined && panelRequest.selection === panelSelection) return;
    const request = { session: id, selection: ++panelSelection, error: undefined as string | undefined };
    const navigation = revision;
    const current = () => panelRequest === request && request.selection === panelSelection &&
      navigation === revision && source === api && !openingSession;
    panelRequest = request;
    renderSessions();
    try {
      const { view } = await directory.refresh(id);
      if (!current()) return;
      if (view.session_id !== id || !view.default_flow) {
        request.error = "No default Flow yet. Open the Session to start a conversation.";
        return;
      }
      const target = source.session(id).flow(view.default_flow);
      if (activeFlow && flowKey(activeFlow) === flowKey(target)) return;
      if (sidebarOpen) {
        setSidebar(false);
        request.selection = panelSelection;
        // Wait for the drawer to release workspace inert before moving focus into it.
        await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
        if (!current()) return;
      }
      if (!openFlowPanel(target, label, false, view)) {
        request.error = "Close a Flow panel to open another.";
        if (narrowProjects.matches) setSidebar(true);
        request.selection = panelSelection;
      }
    } catch (error) {
      if (current()) request.error = message(error);
    } finally {
      if (panelRequest === request) {
        const show = current();
        if (!show || request.error === undefined) panelRequest = undefined;
        renderSessions();
      }
    }
  }

  function closeSession(id: string): void {
    mainSessions.delete(id);
    if (panelRequest?.session === id) {
      panelSelection++;
      panelRequest = undefined;
    }
    for (const key of windowReads.keys()) if (key.startsWith(id + "/")) windows.close(key);
    if (selected?.session_id !== id) {
      if (!requestedLegacy && requestedSession === id) {
        revision++;
        if (!selected) {
          requestWorkspaceLanding();
          return;
        }
        requestedSession = selected.session_id;
        setOpening(false);
        setStatus("");
      }
      renderSessions();
      return;
    }
    const pending = openingSession && requestedSession !== id ? requestedSession : undefined;
    revision++;
    requestWorkspaceLanding();
    const next = pending ?? [...mainSessions.keys()].at(-1);
    if (next !== undefined) void selectSession(next);
  }

  function requestWorkspaceLanding(): void {
    requestedSession = undefined;
    requestedLegacy = false;
    selected = undefined;
    selectedEdition = undefined;
    closeConversation();
    remember("session");
    setOpening(false);
    setStatus("");
    tabs.replaceChildren();
    targetTitle.textContent = "No Session selected";
    renderSessions();
    renderWorkspaceLanding();
  }

  function openFlowPanel(flow: FlowClient, label: string, reportLimit = true, owner = selected): boolean {
    if (openingSession || !conversationSupported || (activeFlow && flowKey(activeFlow) === flowKey(flow))) return false;
    panelSelection++;
    const key = flowKey(flow);
    const opened = windows.open(key, label, (pane, feedback) => {
      const read = new AbortController();
      windowReads.set(key, read);
      installConversation(flow, pane, read, (text, loading, failed) => {
        feedback.classList.toggle("error", failed ?? false);
        feedback.replaceChildren(
          ...(loading ? [element("span", "loading-mark")] : []),
          document.createTextNode(text),
        );
      }, label, () => {
        if (owner?.session_id !== flow.address.session_id || panelSessions.get(key) === owner) return;
        panelSessions.set(key, owner);
        sidebarProjects.set(owner.project_root ?? "", true);
        renderSessions();
      }, owner);
      return () => {
        read.abort();
        windowReads.delete(key);
        if (panelSessions.delete(key) && api !== undefined) renderSessions();
      };
    });
    if (opened) {
      const pane = windows.activeContent();
      if (pane) showConversationTarget(pane);
    }
    if (!opened && reportLimit) setStatus("Close a Flow panel to open another.", false, true);
    return opened;
  }

  function installConversation(flow: FlowClient, pane: HTMLElement, read: AbortController,
    notify: (text: string, loading?: boolean, failed?: boolean) => void, label: string, verified?: () => void, view?: Dtos["SessionView"]): void {
    const key = flowKey(flow);
    const cached = views.get(key);
    views.delete(key);
    read.signal.addEventListener("abort", () => { activityTargets.delete(read); }, { once: true });
    showConversation(flow, pane, notify, read.signal, {
      ...(cached === undefined ? {} : { cached }),
      verified: () => {
        if (read.signal.aborted) return;
        activityTargets.set(read, { flow, label, ...(view === undefined ? {} : { view }) });
        syncActivity();
        verified?.();
      },
      executionChanged: () => {
        activity.invalidate(flow);
        approvals.invalidate(flow);
        overview.invalidate(flow);
      },
      live: draftsSupported,
      ...(approvalsSupported ? { approvals: {
        changes: approvalChanges,
        current: () => activityTargets.get(read)?.flow === flow &&
          (activeWindow === undefined ? conversationRead : windowReads.get(activeWindow)) === read
          ? approvalView : undefined,
        review: (view, request, trigger) => {
          if (read.signal.aborted || openingSession) return;
          const next = approvals.reviewTool(flow, view, request);
          if (next) openApprovals(next, trigger);
        },
      } } : {}),
      ...(inbox === undefined ? {} : { inbox }),
      ...(historySupported ? { history } : {}),
      active: () => !openingSession && !read.signal.aborted &&
        (activeWindow === undefined ? conversationRead : windowReads.get(activeWindow)) === read,
      settingsFocus,
      ...(preferenceScope === undefined ? {} : { preferenceReceipt: { archive: preferenceArchive, scope: preferenceScope } }),
      draftFor: (page) => draftFor(flow, page),
      console: (snapshot, actions) => input.publish(read, snapshot, actions),
      retain: (memory) => {
        views.delete(key);
        views.set(key, memory);
        if (views.size > 12) {
          const oldest = views.keys().next().value;
          if (oldest !== undefined) views.delete(oldest);
        }
      },
    });
  }

  async function bootstrap(start: HTMLButtonElement): Promise<void> {
    const source = api;
    const original = selected;
    if (source === undefined || original === undefined || start.disabled) return;
    const request = revision;
    start.disabled = true;
    setStatus("Preparing conversation", true);
    try {
      await source
        .session(original.session_id)
        .bootstrap({ expected_catalog: original.catalog });
      if (api === source && request === revision)
        await selectSession(original.session_id);
    } catch (error) {
      if (api === source && request === revision)
        setStatus(message(error), false, true);
    } finally {
      start.disabled = false;
    }
  }

  function dialog(title: string) {
    const node = element("dialog");
    const form = element("form", "dialog-form");
    const heading = element("div", "dialog-heading");
    heading.append(
      element("h2", "", title),
      button("×", "Close", () => node.close(), "icon"),
    );
    const error = element("p", "dialog-error");
    error.setAttribute("role", "alert");
    form.append(heading);
    node.append(form);
    document.body.append(node);
    return { node, form, error };
  }

  function field(name: string, placeholder: string, type = "text") {
    const label = element("label", "", name);
    const input = element("input");
    input.type = type;
    input.placeholder = placeholder;
    label.append(input);
    return { label, input };
  }

  const auth = dialog("Connect to this host");
  const settings = dialog("Settings");
  settings.node.setAttribute("aria-label", "Settings");
  const coAuthorRow = element("div", "setting-switch-row");
  const coAuthor = button("…", "Commit co-author", () => {
    if (coAuthorValue !== undefined) void loadCoAuthor(!coAuthorValue);
  });
  coAuthor.setAttribute("role", "switch");
  coAuthor.setAttribute("aria-checked", "false");
  const coAuthorStatus = element("p", "setting-switch-status");
  coAuthorStatus.setAttribute("role", "status");
  const refreshSettings = button("Refresh", "Refresh settings", () => void loadCoAuthor());
  refreshSettings.hidden = true;
  coAuthorRow.append(element("span", "", "Commit co-author"), coAuthor);
  settings.form.append(coAuthorRow, coAuthorStatus, refreshSettings);
  let coAuthorValue: boolean | undefined;
  let settingsRequest: AbortController | undefined;

  async function loadCoAuthor(next?: boolean): Promise<void> {
    const source = api;
    if (!source || settingsRequest || settingsButton.hidden) return;
    const request = new AbortController();
    settingsRequest = request;
    if (next === undefined) coAuthorValue = undefined;
    coAuthor.disabled = true;
    coAuthorRow.hidden = coAuthorValue === undefined;
    refreshSettings.hidden = true;
    coAuthorStatus.textContent = next === undefined ? "Loading…" : "Saving…";
    const isCurrent = () => api === source && settingsRequest === request && !request.signal.aborted;
    try {
      const options = { signal: request.signal };
      const result = await (next === undefined ? source.gitCoAuthor(options) : source.setGitCoAuthor(next, options));
      if (!isCurrent()) return;
      coAuthorValue = result.enabled;
      coAuthorStatus.textContent = "Applies to all Sessions on this host.";
    } catch {
      if (!isCurrent()) return;
      coAuthorValue = undefined;
      coAuthorStatus.textContent = next === undefined ? "Could not load settings." : "Save unconfirmed. Refresh to check the current setting.";
      refreshSettings.hidden = false;
    } finally {
      if (isCurrent()) {
        settingsRequest = undefined;
        coAuthor.disabled = coAuthorValue === undefined;
        coAuthorRow.hidden = coAuthorValue === undefined;
        coAuthor.textContent = coAuthorValue ? "On" : "Off";
        coAuthor.setAttribute("aria-checked", String(coAuthorValue === true));
      }
    }
  }
  const credential = field("Daemon token", "Paste auth_token", "password");
  credential.input.autocomplete = "off";
  credential.input.required = true;
  const connectButton = element("button", "primary", "Connect");
  connectButton.type = "submit";
  auth.form.append(
    element(
      "p",
      "dialog-copy",
      "Use the auth_token from this host’s daemon.toml. This tab remembers your connection across refreshes.",
    ),
    credential.label,
    auth.error,
    connectButton,
  );
  auth.form.addEventListener("submit", (event) => {
    event.preventDefault();
    void connect(credential.input.value.trim());
  });

  async function connect(token: string): Promise<void> {
    const attempt = ++connectionRevision;
    connectionRead?.abort();
    const connection = new AbortController();
    connectionRead = connection;
    connectButton.disabled = true;
    auth.error.textContent = "";
    connectionState.textContent = "Connecting…";
    if (!auth.node.open) empty("Connecting…", "");
    let unauthorized = false;
    try {
      const transport = new HttpClient({
        endpoint: `${location.origin}/`,
        token,
        contract,
        fetch: async (input, init) => {
          const response = await fetch(input, init);
          if (response.status === 401) {
            unauthorized = true;
            if (attempt === connectionRevision && api !== undefined) {
              disconnect("The connection credential has expired or is invalid.");
            }
          }
          return response;
        },
      });
      const source = new AtmanClient(transport);
      const negotiated = await source.capabilities({ signal: connection.signal });
      if (attempt !== connectionRevision) return;
      if ((originalCredential ?? saved("token")) !== token) {
        remember("session");
        remember("flows");
        remember("draftScope");
        drafts.clear();
      }
      originalCredential = token;
      const storedScope = saved("draftScope");
      try {
        draftScope = storedScope === undefined ? crypto.randomUUID() : canonicalUuid(storedScope);
      } catch {
        draftScope = crypto.randomUUID();
      }
      remember("draftScope", draftScope);
      api = source;
      flowCatalog = new SessionCatalog(source, connection.signal);
      remember("token", token);
      conversationSupported =
        negotiated.methods["flow.conversation"]?.revision === 1;
      projectsSupported = negotiated.methods["project.list"]?.revision === 1;
      projectPinSupported = projectsSupported && negotiated.methods["project.set_pinned"]?.revision === 1;
      overviewSupported = negotiated.methods["flow.overview"]?.revision === 1;
      activitySupported = negotiated.methods["flow.execution"]?.revision === 1;
      approvalsSupported = negotiated.methods["flow.approval_queue"]?.revision === 1;
      if (approvalsSupported) approvals.connect(negotiated.methods.resolve_permission_requests?.revision === 1);
      draftsSupported = negotiated.methods["flow.drafts"]?.revision === 1;
      const available = (method: string) => negotiated.methods[method]?.revision === 1;
      settingsButton.hidden = !["settings.git_co_author", "settings.git_co_author.set"].every(available);
      preferenceScope = ["flow.composer.prepare", "flow.composer.preference.set", "flow.composer.preference.status"].every(available)
        ? draftScope : undefined;
      if (preferenceScope) preferenceArchive.connect(preferenceScope);
      inbox = conversationSupported && available("flow.inbox") ?
        { pending: new Map(), edits: available("flow.inbox_edit") } : undefined;
      historySupported = conversationSupported && ["flow.events", "flow.history_branches", "flow.history_review",
        "flow.history_begin", "flow.history_stop", "flow.history_apply", "flow.history_abort", "flow.history_status"].every(available);
      if (historySupported) history.connect(draftScope, connection.signal, available("flow.history_recover_input"));
      if (activitySupported) activity.connect(draftScope, negotiated.methods["flow.control"]?.revision === 1);
      credential.input.value = "";
      connectionState.textContent = "Connected";
      auth.node.close();
      refreshDirectory();
      const lastSession = saved("session");
      if (lastSession !== undefined) void selectSession(lastSession);
    } catch (error) {
      if (attempt !== connectionRevision) return;
      connectionState.textContent = "Disconnected";
      if (unauthorized && saved("token") === token) remember("token");
      if (auth.node.open || unauthorized) {
        renderDisconnected();
        if (!auth.node.open) auth.node.showModal();
        auth.error.textContent = unauthorized ? "The daemon token is invalid." : message(error);
      } else {
        empty(
          "Couldn’t reconnect",
          message(error),
          button("Retry", "Retry connection", () => void connect(token), "primary"),
          button("Enter token", "Connect to this host", () => auth.node.showModal()),
        );
      }
    } finally {
      if (attempt === connectionRevision) connectButton.disabled = false;
    }
  }

  const creation = dialog("New Session");
  const createDialog = creation.node;
  const title = field("Title", "Untitled Session");
  const directory = field("Project directory", "Host directory (optional)");
  const createButton = element("button", "primary", "Create");
  createButton.type = "submit";
  creation.form.append(
    title.label,
    directory.label,
    creation.error,
    createButton,
  );
  function openCreateSession(project?: string): void {
    if (api === undefined) return;
    if (!pendingCreate) directory.input.value = project ?? "";
    if (!createDialog.open) createDialog.showModal();
  }
  let pendingCreate:
    | {
        source: AtmanClient;
        selection: number;
        request: Dtos["CreateSessionRequest"];
      }
    | undefined;
  creation.form.addEventListener("submit", (event) => {
    event.preventDefault();
    const source = api;
    if (source === undefined) return;
    if (pendingCreate === undefined) {
      const project = directory.input.value.trim();
      pendingCreate = {
        source,
        selection: revision,
        request: {
          session_id: crypto.randomUUID(),
          title: title.input.value.trim() || "Untitled Session",
          ...(project ? { project_root: project } : {}),
        },
      };
    }
    const original = pendingCreate;
    if (original.source !== source) return;
    createButton.disabled = true;
    title.input.readOnly = true;
    directory.input.readOnly = true;
    creation.error.textContent = "";
    void source
      .createSession(original.request)
      .then(async () => {
        if (api !== source || pendingCreate !== original) return;
        pendingCreate = undefined;
        createDialog.close();
        title.input.value = "";
        directory.input.value = "";
        title.input.readOnly = false;
        directory.input.readOnly = false;
        createButton.textContent = "Create";
        refreshDirectory();
        if (original.selection === revision) {
          setSidebar(false);
          await selectSession(original.request.session_id, true);
        }
      })
      .catch((error: unknown) => {
        if (api !== source || pendingCreate !== original) return;
        creation.error.textContent = message(error);
        if (error instanceof ClientError && error.outcome === "not_sent") {
          pendingCreate = undefined;
          title.input.readOnly = false;
          directory.input.readOnly = false;
        } else createButton.textContent = "Retry original creation";
      })
      .finally(() => {
        if (api === source) createButton.disabled = false;
      });
  });

  function disconnect(reason = ""): void {
    connectionRevision++;
    settingsRequest?.abort();
    settingsRequest = undefined;
    coAuthorValue = undefined;
    settings.node.close();
    settingsButton.hidden = true;
    overviewSupported = false;
    overview.select(undefined);
    activitySupported = false;
    draftsSupported = false;
    historySupported = false;
    inbox = undefined;
    history.disconnect();
    activity.disconnect();
    approvalsSupported = false;
    approvals.disconnect();
    remember("token");
    if (!reason) {
      remember("session");
      remember("flows");
      remember("draftScope");
      originalCredential = undefined;
    }
    connectionRead?.abort();
    connectionRead = undefined;
    flowCatalog = undefined;
    api = undefined;
    credential.input.value = "";
    auth.error.textContent = "";
    connectButton.disabled = false;
    revision++;
    listingRead?.abort();
    listingRead = undefined;
    projectRead?.abort();
    projectRead = undefined;
    sidebar.removeAttribute("aria-busy");
    search.value = "";
    setSidebar(false);
    setOpening(false);
    requestedSession = undefined;
    requestedLegacy = false;
    selected = undefined;
    selectedEdition = undefined;
    panelRequest = undefined;
    closeConversation();
    windows.reset();
    mainSessions.clear();
    draftScope = undefined;
    preferenceScope = undefined;
    if (!reason) drafts.clear();
    views.clear();
    conversationSupported = false;
    pendingCreate = undefined;
    title.input.readOnly = false;
    directory.input.readOnly = false;
    createButton.disabled = false;
    createButton.textContent = "Create";
    createDialog.close();
    sessionRows = undefined;
    projectPage = undefined;
    projectsSupported = false;
    projectPinSupported = false;
    pinWrite = undefined;
    pinError = undefined;
    projectError = undefined;
    catalogError = undefined;
    sidebarProjects.clear();
    workspaceProjects.clear();
    sessions.replaceChildren();
    tabs.replaceChildren();
    targetTitle.textContent = "No Session selected";
    connectionState.textContent = "Disconnected";
    setStatus("");
    renderDisconnected();
    if (!auth.node.open) auth.node.showModal();
    auth.error.textContent = reason;
  }

  function renderDisconnected(): void {
    empty(
      "Your workspace",
      "Connect to browse this host’s Sessions.",
      button(
        "Connect",
        "Connect to this host",
        () => auth.node.showModal(),
        "primary",
      ),
    );
  }

  const listener = { signal: lifetime.signal };
  window.addEventListener("focus", () => settingsFocus.dispatchEvent(new Event("change")), listener);
  narrowProjects.addEventListener("change", updateSidebarToggle, listener);
  updateSidebarToggle();
  sidebarToggle.addEventListener("click", () => {
    if (!narrowProjects.matches) setDesktopSidebar(!desktopSidebarOpen);
    else if (sidebarOpen) setSidebar(false);
    else focusProjectSearch();
  }, listener);
  disconnectButton.addEventListener("click", () => disconnect(), listener);
  settingsButton.addEventListener("click", () => {
    if (settings.node.open) return;
    settings.node.showModal();
    void loadCoAuthor();
  }, listener);
  settings.form.addEventListener("submit", (event) => event.preventDefault(), listener);
  newSessionButton.addEventListener("click", () => {
    openCreateSession();
  }, listener);
  refreshButton.addEventListener("click", refreshDirectory, listener);
  shell.addEventListener("keydown", (event) => {
    if (auth.node.open || createDialog.open || settings.node.open) return;
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      focusProjectSearch();
    }
  }, listener);

  const savedToken = saved("token");
  if (savedToken === undefined) {
    renderDisconnected();
    auth.node.showModal();
  } else {
    void connect(savedToken);
  }

  return () => {
    connectionRevision++;
    overviewSupported = false;
    overview.select(undefined);
    activitySupported = false;
    activity.disconnect();
    approvalsSupported = false;
    approvals.disconnect();
    revision++;
    api = undefined;
    lifetime.abort();
    listingRead?.abort();
    projectRead?.abort();
    connectionRead?.abort();
    settingsRequest?.abort();
    closeConversation();
    windows.destroy();
    input.destroy();
    history.disconnect();
    if (historyPanel) void unmount(historyPanel);
    historyDialog.remove();
    views.clear();
    auth.node.remove();
    createDialog.remove();
    settings.node.remove();
  };
}
