import {
  AtmanClient,
  ClientError,
  HttpClient,
  canonicalUuid,
  contract,
  cursorId,
} from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import { flushSync, mount, unmount } from "svelte";
import ConversationHistory from "./ConversationHistory.svelte";
import WorkspacePicker from "./WorkspacePicker.svelte";
import { HistoryController, sameHistoryHead } from "./history-controller.ts";
import { showConversation } from "./conversation.ts";
import { FlowOverview } from "./flow-overview.ts";
import { ExecutionActivity } from "./execution-activity.ts";
import { ApprovalActivity } from "./approval-activity.ts";
import type { ApprovalView } from "./approval-activity.ts";
import { flowWindows } from "./flow-windows.ts";
import { ProjectPreviews } from "./project-previews.ts";
import { ProjectDirectory } from "./project-directory.ts";
import type { ProjectSort } from "./project-directory.ts";
import type { PendingInbox } from "./inbox.ts";
import { sharedConsole } from "./shared-console.ts";
import type { ConversationDraftView, ConversationMemory, ConversationSeed, Draft } from "./conversation.ts";
import { SessionCatalog, initialFlow, mergeFlowPages } from "./session-catalog.ts";
import type { SessionEdition } from "./session-catalog.ts";
import type { WorkbenchElements } from "./shell-elements.ts";
import { PreferenceReceipts } from "./preference-editor.ts";
import { bindDraft, TabDrafts, type DraftContent } from "./tab-drafts.ts";
import { QuestionWindow, questionKeepMethods, questionLiveMethod, questionMethods, questionModelMethods, questionRefreshMethods, questionStopMethods } from "./question-window.ts";
import { QuestionDrafts } from "./question-drafts.ts";

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

async function includesReceipt(flow: FlowClient, shown: Dtos["FlowCursor"], receipt: Dtos["FlowCursor"], signal: AbortSignal): Promise<boolean> {
  if (shown.journal_id !== receipt.journal_id || shown.sequence < receipt.sequence) return false;
  if (shown.sequence === receipt.sequence) return cursorId(shown) === cursorId(receipt);
  const { result } = await flow.events({ after: receipt, limit: 1 }, { signal });
  return result.batches[0]?.receipt.sequence === receipt.sequence + 1;
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
    newProjectButton,
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
  let artifactsSupported = false;
  let imagesSupported = false;
  let projectPreviewSupported = false;
  let previewFocus: AbortController | undefined;
  let historySupported = false;
  let questionsSupported = false;
  let questionControls = { stop: false, refresh: false, keep: false, models: false, live: false };
  const questionWindows = new Map<string, QuestionWindow>();
  const questionDrafts = new QuestionDrafts(tabStorage());
  let inbox: { pending: Map<string, PendingInbox>; edits: boolean } | undefined;
  const activityTargets = new Map<AbortController, {
    flow: FlowClient; label: string; view?: Dtos["SessionView"]; pane: HTMLElement; draft: ConversationDraftView;
  }>();

  function syncActivity(): void {
    const read = activeWindow === undefined ? conversationRead : windowReads.get(activeWindow);
    const target = !openingSession && read !== undefined ? activityTargets.get(read) : undefined;
    const question = activeWindow !== undefined && questionWindows.has(activeWindow);
    if (projectPreviewSupported && !openingSession) {
      const verified = target && target.view && (activeWindow === undefined || panelSessions.has(activeWindow)) ? read : undefined;
      if (verified !== previewFocus) {
        previewFocus = verified;
        if (verified && target) projectPreviews.focus(target.flow);
        else projectPreviews.unfocus();
      }
    }
    const pending = api !== undefined && (openingSession || (read !== undefined && target === undefined && !question));
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
  const mainPreviewFlows = new Map<string, FlowClient>();
  const previewRows = new Map<string, HTMLButtonElement>();
  const previewRowId = new WeakMap<Element, string>();
  const visiblePreviews = new Set<string>();
  const previewChanges = new Set<string>();
  let previewFrame: number | undefined;
  const projectPreviews = new ProjectPreviews((id) => {
    previewChanges.add(id);
    if (previewFrame !== undefined) return;
    previewFrame = requestAnimationFrame(() => {
      previewFrame = undefined;
      for (const session of previewChanges) {
        const row = previewRows.get(session);
        if (row?.isConnected) paintProjectPreview(row, session);
      }
      previewChanges.clear();
    });
  }, () => (narrowProjects.matches ? sidebarOpen : desktopSidebarOpen) ? visiblePreviews : new Set());
  const previewVisibility = new IntersectionObserver((observations) => {
    for (const { target, isIntersecting } of observations) {
      const id = previewRowId.get(target);
      if (id === undefined || previewRows.get(id) !== target) continue;
      if (isIntersecting) visiblePreviews.add(id);
      else visiblePreviews.delete(id);
    }
    projectPreviews.wake();
  }, { root: sessions });

  function clearPreviewRows(): void {
    previewVisibility.disconnect();
    previewRows.clear();
    visiblePreviews.clear();
  }

  function retireProjectPreviews(): void {
    projectPreviewSupported = false;
    previewFocus = undefined;
    projectPreviews.close();
    mainPreviewFlows.clear();
    clearPreviewRows();
    previewChanges.clear();
    if (previewFrame !== undefined) cancelAnimationFrame(previewFrame);
    previewFrame = undefined;
  }

  function retainProjectPreview(flow: FlowClient): void {
    if (!projectPreviews.target(flow.address.session_id)) projectPreviews.keep(flow);
  }

  function detachProjectPanel(flow: FlowClient): void {
    const id = flow.address.session_id;
    if (projectPreviews.target(id)?.address.instance_id !== flow.address.instance_id) return;
    const panel = [...panelSessions].reverse().find(([, owner]) => owner.session_id === id);
    const read = panel && windowReads.get(panel[0]);
    const sibling = mainPreviewFlows.get(id) ?? (read && activityTargets.get(read)?.flow);
    if (sibling) projectPreviews.keep(sibling);
    else projectPreviews.remove(id);
  }

  function openedSessions(): Map<string, OpenSession> {
    const opened = new Map<string, OpenSession>();
    for (const view of panelSessions.values()) opened.set(view.session_id, view);
    for (const view of mainSessions.values()) opened.set(view.session_id, view);
    return opened;
  }

  type ProjectPage = Dtos["ProjectListPage"];
  type ProjectGroup = { root: string; label: string; pinned: boolean; record?: Dtos["ProjectListEntry"]; count?: number; sessions: SessionRow[] };
  let sessionRows: readonly SessionRow[] | undefined;
  let projectPage: ProjectPage | undefined;
  let projectsSupported = false;
  let directorySupported = false;
  let projectPinSupported = false;
  let pinWrite: { fingerprint: string } | undefined;
  let pinError: { fingerprint: string; message: string } | undefined;
  let projectError: string | undefined;
  let catalogError: string | undefined;
  const sidebarProjects = new Map<string, boolean>();
  const workspaceProjects = new Map<string, boolean>();
  const projectDirectory = new ProjectDirectory(() => {
    if (projectDirectory.loadingQuery) sidebar.setAttribute("aria-busy", "true");
    else sidebar.removeAttribute("aria-busy");
    renderSessions();
    renderWorkspaceLanding();
  });
  const workspacePicker = mount(WorkspacePicker, { target: document.body, props: {
    storage: tabStorage(),
    captureNavigation: () => {
      const source = api;
      const session = revision;
      const panel = panelSelection;
      return () => source === api && session === revision && panel === panelSelection && !openingSession;
    },
    install: installWorkspace,
    pinObserved: refreshDirectory,
    hasPending: (pending) => {
      const label = pending ? "Resume opening workspace" : "New Project";
      newProjectButton.setAttribute("aria-label", label);
      newProjectButton.title = label;
      newProjectButton.classList.toggle("workspace-pending", pending);
    },
    focusConversation: () => content.focus({ preventScroll: true }),
  } });

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
      ...(restored?.recovery === undefined ? {} : { recovery: restored.recovery }),
      changes: new EventTarget(),
    };
    if (binding) {
      draft.persist = () => {
        return draftArchive.save(binding, {
          text: draft.text,
          edit: draft.edit,
          ...(draft.attachments === undefined ? {} : { attachments: draft.attachments }),
          ...(draft.pending === undefined ? {} : { pending: draft.pending }),
          ...(draft.recovery === undefined ? {} : { recovery: draft.recovery }),
        });
      };
    }
    drafts.set(key, draft);
    return draft;
  }

  function setOpening(opening: boolean, sync = true): void {
    const wasOpening = openingSession;
    openingSession = opening;
    if (wasOpening !== opening) settingsFocus.dispatchEvent(new Event("change"));
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
    if (projectPreviewSupported) projectPreviews.wake();
  }

  function updateSidebarToggle(): void {
    sidebarToggle.setAttribute("aria-expanded", String(narrowProjects.matches ? sidebarOpen : desktopSidebarOpen));
  }

  function setDesktopSidebar(open: boolean): void {
    if (desktopSidebarOpen !== open) panelSelection++;
    desktopSidebarOpen = open;
    shell.classList.toggle("projects-collapsed", !open);
    updateSidebarToggle();
    if (projectPreviewSupported) projectPreviews.wake();
  }

  function focusProjectSearch(): void {
    if (narrowProjects.matches) setSidebar(true);
    else setDesktopSidebar(true);
    requestAnimationFrame(() => {
      if (!lifetime.signal.aborted) search.focus({ preventScroll: true });
    });
  }

  function updateSearch(): void {
    if (directorySupported) {
      sessions.scrollTop = 0;
      projectDirectory.search(search.value);
      return;
    }
    renderSessions();
    renderWorkspaceLanding();
  }

  search.maxLength = 256;
  search.addEventListener("input", updateSearch, { signal: lifetime.signal });
  sidebar.addEventListener("keydown", (event) => {
    const origin = event.target;
    if (origin !== search && !(origin instanceof HTMLElement && origin.classList.contains("session-row"))) return;
    if (!["ArrowDown", "ArrowUp", "Enter"].includes(event.key) || event.isComposing) return;
    if (event.key === "Enter" && origin !== search) return;
    if (directorySupported && projectDirectory.loadingQuery) return;
    const choices = [...sessions.querySelectorAll<HTMLButtonElement>(".session-row")]
      .filter((row) => row.getClientRects().length > 0);
    if (!choices.length) return;
    const index = choices.findIndex((row) => row === origin);
    if (event.key === "Enter") choices[0]?.click();
    else if (index === 0 && event.key === "ArrowUp") search.focus();
    else choices[index < 0 ? (event.key === "ArrowUp" ? choices.length - 1 : 0) : index + (event.key === "ArrowUp" ? -1 : 1)]?.focus();
    event.preventDefault();
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

  function paintProjectPreview(row: HTMLButtonElement, id: string): void {
    const preview = row.querySelector<HTMLElement>(".session-preview");
    if (!preview) return;
    const { text, working, unread } = projectPreviews.view(id);
    if (preview.textContent !== text) preview.textContent = text;
    row.dataset.working = String(working);
    row.dataset.unread = unread ?? "";
    const outcome = unread === undefined ? (working ? "Working" : "") : {
      completed: "Completed · unread", cancelled: "Cancelled · unread",
      failed: "Failed · unread", host_lost: "Interrupted · unread",
    }[unread];
    const description = [outcome, text].filter(Boolean).join(" · ");
    row.title = [row.getAttribute("aria-label"), description].filter(Boolean).join(" · ");
    if (description) row.setAttribute("aria-description", description);
    else row.removeAttribute("aria-description");
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
    } else if (allowPanel && opened && projectPreviewSupported) {
      copy.append(element("span", "session-preview"));
      previewRows.set(row.id, node);
      previewRowId.set(node, row.id);
      previewVisibility.observe(node);
    } else if (row.status !== "idle") {
      copy.append(element("span", "session-meta", row.status));
    }
    node.append(element("span", "session-marker"), copy);
    paintProjectPreview(node, row.id);
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
    for (const group of projectDirectory.groups.value?.rows ?? []) {
      const root = group.project_root ?? "";
      const page = projectDirectory.sessions(root)?.value;
      groups.set(root, {
        root,
        label: group.label,
        pinned: group.record?.pinned ?? false,
        ...(group.record == null ? {} : { record: group.record }),
        count: page?.page.total ?? group.session_count,
        sessions: (page?.rows ?? group.preview).map((row): SessionRow => ({
          id: row.id, title: row.title, project_root: root || null,
          status: row.available ? "idle" : "unavailable", ...(row.kind === "native" ? { persistent: {} } : {}),
        })),
      });
    }
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
    for (const group of groups.values()) for (const row of group.sessions) if (row.persistent != null) missing.delete(row.id);
    for (const view of missing.values()) {
      if (directorySupported && query && ![view.title ?? "", view.project_root ?? "", view.session_id,
        groups.get(view.project_root ?? "")?.label ?? ""].some((value) => value.toLowerCase().includes(query))) continue;
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
      if (directorySupported || !query || [project.label, project.root].some((value) => value.toLowerCase().includes(query))) {
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
        element("span", "project-count", String(project.count ?? project.sessions.length)),
      );
      const list = element("div", "project-rows");
      const current = project.sessions.some(isCurrentSession);
      summary.classList.toggle("contains-current", current);
      if (!searching && current && !expanded.has(project.root)) expanded.set(project.root, true);
      group.open = searching || (expanded.get(project.root) ?? (current || projects.length === 1));
      const showRows = () => {
        if (list.hasChildNodes()) return;
        const isOpen = (row: SessionRow) => row.persistent != null && opened?.has(row.id) === true;
        const page = projectDirectory.sessions(project.root);
        const modern = directorySupported && project.count !== undefined;
        if (modern && !page && project.count && projectDirectory.sort(project.root) !== "newest_first") {
          queueMicrotask(() => {
            if (group.isConnected && group.open && !projectDirectory.sessions(project.root)) void projectDirectory.loadSessions(project.root);
          });
        }
        const visible = modern
          ? [...project.sessions.filter(isOpen), ...project.sessions.filter((row) => !isOpen(row))]
          : opened === undefined || searching ? project.sessions : sidebarSessionRows(project.sessions, isOpen);
        if (modern && project.count !== 0) {
          const sort = element("select", "project-session-sort");
          sort.dataset.project = project.root;
          sort.dataset.action = "sort-sessions";
          sort.setAttribute("aria-label", `Sort Sessions in ${project.label}`);
          for (const [value, label] of [["newest_first", "Recent"], ["oldest_first", "Oldest"], ["name_ascending", "Name"]] as const) {
            const choice = element("option", "", label);
            choice.value = value;
            sort.append(choice);
          }
          sort.value = projectDirectory.sort(project.root);
          sort.addEventListener("change", () => projectDirectory.setSort(project.root, sort.value as ProjectSort));
          const controls = element("div", "project-session-controls");
          controls.append(sort);
          if (page?.read) {
            const spinner = element("span", "loading-mark");
            spinner.setAttribute("role", "status");
            spinner.setAttribute("aria-label", "Loading Sessions");
            controls.append(spinner);
          }
          list.append(controls);
        }
        if (visible.length) list.append(...visible.map((row) => sessionRow(row, opened !== undefined, isOpen(row))));
        else if (modern || sessionRows !== undefined) list.append(element("p", "sidebar-message", "No Sessions"));
        if (modern) {
          if (page?.error) list.append(element("p", "sidebar-message error", page.error));
          const more = page?.value ? page.value.page.after != null : (project.count ?? 0) > 5;
          if (more || page?.error) {
            const action = page?.error ? "Retry" : "More Sessions";
            const next = button(action, `${action} in ${project.label}`, () =>
              void projectDirectory.loadSessions(project.root, page?.value?.page.after != null), "project-session-more");
            next.dataset.project = project.root;
            next.dataset.action = "more-sessions";
            next.setAttribute("aria-disabled", String(page?.read !== undefined));
            list.append(next);
          }
        }
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
    if (directorySupported) {
      const feed = projectDirectory.groups;
      if (feed.error) return [element("p", "sidebar-message error", feed.error),
        button("Retry Projects", "Reload the Project directory", () => void projectDirectory.loadGroups(), "directory-more")];
      if (feed.value?.page.after != null) {
        const more = button("…", "Load more Projects", () => {
          if (!pinWrite) void projectDirectory.loadGroups(true);
        }, "directory-more");
        more.setAttribute("aria-disabled", String(feed.read !== undefined || pinWrite !== undefined));
        return [more];
      }
      if (projectDirectory.loadingQuery && feed.value === undefined) {
        const loading = element("p", "sidebar-message project-loading");
        loading.setAttribute("role", "status");
        loading.append(element("span", "loading-mark"), document.createTextNode("Loading Projects…"));
        return [loading];
      }
      return [];
    }
    if (projectError !== undefined) {
      return [element("p", "sidebar-message error", projectError),
        button("Retry Projects", "Reload the Project directory", () => void loadProjects(), "directory-more")];
    }
    if (projectPage?.after == null) return [];
    const more = button("…", "Load more Projects", () => void loadProjects(true), "directory-more");
    more.disabled = projectRead !== undefined || pinWrite !== undefined;
    return [more];
  }

  function preserveDirectoryPosition(container: HTMLElement, fallback: HTMLElement): () => void {
    const focused = document.activeElement as HTMLElement | null;
    const focus = container.contains(focused) ? focused?.dataset : undefined;
    const onMore = container.contains(focused) && focused?.matches(".directory-more");
    const scroll = container.scrollTop;
    return () => {
      container.scrollTop = scroll;
      const session = focus?.session === undefined ? [] : [...container.querySelectorAll<HTMLElement>("[data-session]")]
        .filter((row) => row.dataset.session === focus.session);
      const project = focus?.project === undefined ? [] : [...container.querySelectorAll<HTMLElement>("[data-project]")]
        .filter((row) => row.dataset.project === focus.project);
      const target = focus?.session !== undefined
        ? session.find((row) => row.dataset.action === focus.action) ?? session.find((row) => row.dataset.action === undefined) ?? fallback
        : focus?.project !== undefined
          ? project.find((row) => row.dataset.action === focus.action) ?? project.find((row) => row.dataset.action === undefined) ?? fallback
          : onMore ? container.querySelector<HTMLElement>(".directory-more") ?? fallback : undefined;
      target?.focus({ preventScroll: true });
    };
  }

  function renderSessions(): void {
    const restore = preserveDirectoryPosition(sessions, search);
    clearPreviewRows();
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
    } else if (projects.length === 0 && (directorySupported ? projectDirectory.groups.value !== undefined : sessionRows !== undefined) && !projectRead &&
        projectError === undefined && projectPage?.after == null) {
      sessions.append(
        element(
          "p",
          "sidebar-message",
          searching ? "No matches" : "No Projects",
        ),
      );
    }
    restore();
  }

  function renderWorkspaceLanding(): void {
    if (requestedSession !== undefined) return;
    const restore = preserveDirectoryPosition(content, content);

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
    } else if ((directorySupported ? projectDirectory.groups.value === undefined : sessionRows === undefined) || projectRead !== undefined) {
      empty("Loading Projects…", "");
    } else if (searching) {
      empty(
        "No matching Sessions",
        "Try a different search or start a new Session.",
        create,
        button("Clear search", "Clear Session search", () => {
          search.value = "";
          updateSearch();
        }),
      );
    } else {
      empty("No Sessions yet", "Create a Session to start a conversation.", create);
    }
    restore();
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
    const observed = directorySupported ? projectDirectory.catalogFor(record) : page?.projects.includes(record) ? page.revision : undefined;
    if (!source || !projectPinSupported || !observed || pinWrite) return;
    const write = { fingerprint: record.fingerprint };
    pinWrite = write;
    pinError = undefined;
    renderSessions();
    try {
      const receipt = await source.setProjectPinned({ fingerprint: record.fingerprint,
        expected_revision: observed, pinned });
      if (api !== source || pinWrite !== write) return;
      // Only replace the observed row. A fresh listing owns paging and any unrelated row changes.
      if (directorySupported) projectDirectory.applyPin(record, receipt);
      else if (projectPage?.revision === observed) {
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
    if (directorySupported) await projectDirectory.refresh();
    else await loadProjects();
    if (api === source && pinWrite === write) {
      pinWrite = undefined;
      renderSessions();
    }
  }

  function refreshDirectory(): void {
    if (directorySupported) {
      void projectDirectory.refresh();
      return;
    }
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

  function installSession(source: AtmanClient, edition: SessionEdition, destination?: {
    flow: FlowClient; seed: ConversationSeed;
  }): HTMLButtonElement | undefined {
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
    if (destination) selectFlow(destination.flow, destination.seed);
    else if (!sameView) {
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

  function selectFlow(flow: FlowClient, seed?: ConversationSeed): void {
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
    () => {
      rememberFlow(flow);
      if (projectPreviewSupported) {
        mainPreviewFlows.set(flow.address.session_id, flow);
        retainProjectPreview(flow);
      }
    }, selected, seed);
    renderSessions();
  }

  function flowKey(flow: FlowClient): string {
    return flow.address.session_id + "/" + flow.address.instance_id;
  }

  async function installWorkspace(source: AtmanClient,
    ready: Extract<Dtos["WorkspaceOpenProgress"], { state: "conversation_ready" }>,
    signal: AbortSignal, current: () => boolean): Promise<boolean> {
    const directory = flowCatalog;
    if (!directory || !current()) return false;
    const id = ready.flow.session_id;
    const session = source.session(id);
    const flow = session.flow(ready.flow.instance_id);
    const [{ result: view }, { result }] = await Promise.all([
      session.open({ signal }),
      flow.conversation({ ...(draftsSupported || artifactsSupported || imagesSupported ? { include_source_parts: true } : {}) }, { signal }),
    ]);
    if (!current()) return false;
    if (view.session_id !== id || view.catalog.journal_id !== ready.catalog.journal_id ||
        view.catalog.sequence < ready.catalog.sequence ||
        (view.catalog.sequence === ready.catalog.sequence && cursorId(view.catalog) !== cursorId(ready.catalog)) ||
        result.state !== "ready" || !await includesReceipt(flow, result.page.source.cursor, ready.receipt, signal)) {
      throw new Error("Workspace Session does not match its opening receipt.");
    }
    const edition = await directory.readFlows(view, signal);
    if (!current()) return false;
    directory.store(edition);
    revision++;
    requestedSession = id;
    requestedLegacy = false;
    installSession(source, edition, { flow, seed: { page: result.page, followingTail: true } });
    setSidebar(false);
    showConversationTarget(content);
    return true;
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
    mainPreviewFlows.delete(id);
    projectPreviews.remove(id);
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
    const opened = windows.open(key, label, (pane, feedback) => mountFlowPanel(flow, label, owner, pane, feedback));
    if (opened) {
      const pane = windows.activeContent();
      if (pane) showConversationTarget(pane);
    }
    if (!opened && reportLimit) setStatus("Close a Flow panel to open another.", false, true);
    return opened;
  }

  function mountFlowPanel(flow: FlowClient, label: string, owner: Dtos["SessionView"] | undefined,
    pane: HTMLElement, feedback: HTMLElement, seed?: ConversationSeed): () => void {
    const key = flowKey(flow);
    const read = new AbortController();
    windowReads.set(key, read);
    const retire = () => {
      read.abort();
      windowReads.delete(key);
      if (panelSessions.delete(key) && api !== undefined) {
        detachProjectPanel(flow);
        renderSessions();
      }
    };
    try {
      installConversation(flow, pane, read, (text, loading, failed) => {
        feedback.classList.toggle("error", failed ?? false);
        feedback.replaceChildren(
          ...(loading ? [element("span", "loading-mark")] : []),
          document.createTextNode(text),
        );
      }, label, () => {
        if (owner?.session_id !== flow.address.session_id || panelSessions.get(key) === owner) return;
        panelSessions.set(key, owner);
        if (projectPreviewSupported) retainProjectPreview(flow);
        sidebarProjects.set(owner.project_root ?? "", true);
        renderSessions();
      }, owner, seed);
      return retire;
    } catch (error) {
      retire();
      throw error;
    }
  }

  function openQuestionWindow(flow: FlowClient, page: Dtos["ConversationPage"], sourceRead: AbortController): void {
    const connection = connectionRead;
    if (!questionsSupported || !connection || connection.signal.aborted || openingSession || sourceRead.signal.aborted) return;
    const key = `${flow.address.session_id}/question/${flow.address.instance_id}/${page.head.branch}`;
    let question = questionWindows.get(key);
    if (!question || question.retired) {
      const binding = draftScope && bindDraft(draftScope, flow.address, page);
      question = new QuestionWindow(flow, { source: page.source, head: page.head }, connection.signal,
        binding ? { drafts: questionDrafts, binding } : undefined, questionControls);
      questionWindows.set(key, question);
    }
    const selectedQuestion = question;
    if (!windows.open(key, "Quick Ask", (pane, feedback) => {
      const read = new AbortController();
      windowReads.set(key, read);
      selectedQuestion.mount(pane, feedback, read.signal, (snapshot, actions) => input.publish(read, snapshot, actions),
        (answer, available) => bringQuestionAnswer(selectedQuestion, key, read, sourceRead, answer, available),
        (receipt, draft, reading) => installSavedQuestion(key, read, receipt, draft, reading));
      return () => {
        const restore = activeWindow === key;
        read.abort();
        selectedQuestion.close();
        windowReads.delete(key);
        if (!selectedQuestion.remains) questionWindows.delete(key);
        if (restore) requestAnimationFrame(() => {
          const focused = activeWindow === undefined ? conversationRead : windowReads.get(activeWindow);
          if (focused === sourceRead && !sourceRead.signal.aborted)
            composer.querySelector<HTMLButtonElement>("button[aria-label='Quick Ask']")?.focus({ preventScroll: true });
        });
      };
    })) {
      if (!selectedQuestion.remains) questionWindows.delete(key);
      setStatus("Close a Flow panel to open another.", false, true);
      return;
    }
    const target = windows.activeContent();
    if (target) showConversationTarget(target);
  }

  async function installSavedQuestion(key: string, questionRead: AbortController, publication: Dtos["QuestionKeepReceipt"],
    draft: DraftContent, reading: () => Pick<ConversationMemory, "scrollTop" | "followingTail">): Promise<boolean> {
    const source = api;
    const signal = questionRead.signal;
    const selection = panelSelection;
    const current = () => api === source && !signal.aborted && !openingSession && activeWindow === key && panelSelection === selection;
    if (!source || !current()) return false;
    const session = source.session(publication.flow.session_id);
    const flow = session.flow(publication.flow.instance_id);
    const destination = flowKey(flow);
    const existingRead = activeFlow && flowKey(activeFlow) === destination ? conversationRead : windowReads.get(destination);
    const existing = existingRead && activityTargets.get(existingRead);
    if (existingRead && !existing) throw new Error("The saved Flow is still opening.");
    const firstPage = async () => {
      const { result } = await flow.conversation({ ...(draftsSupported || artifactsSupported || imagesSupported ? { include_source_parts: true } : {}) }, { signal });
      if (result.state === "ready") return result.page;
    };
    const [page, { result: owner }] = await Promise.all([existing?.draft.check() ?? firstPage(), session.open({ signal })]);
    if (!current()) return false;
    if (!page) throw new Error("Could not read the saved Flow.");
    if (!await includesReceipt(flow, page.source.cursor, publication.destination, signal))
      throw new Error("Saved Flow is missing the original question receipt.");
    if (!current() || existingRead?.signal.aborted) return false;
    const carried = draft.text !== "" || draft.attachments !== undefined;
    const target = draftFor(flow, page);
    const hasDraft = target.text !== "" || target.attachments !== undefined || target.pending !== undefined || target.busy;
    if (carried && hasDraft)
      throw new Error("The saved Flow already has a draft. This question still holds its draft.");

    const label = "Saved Quick Ask";
    if (existing) {
      if (existingRead === conversationRead) windows.main();
      else windows.focus(destination);
      windows.close(key);
      showConversationTarget(existing.pane);
    } else {
      const position = reading();
      const pane = windows.replace(key, destination, label,
        (pane, feedback) => mountFlowPanel(flow, label, owner, pane, feedback, { page, followingTail: position.followingTail }));
      if (!pane?.isConnected) return false;
      pane.scrollTop = position.followingTail ? pane.scrollHeight : position.scrollTop;
    }
    if (carried) {
      target.text = draft.text;
      if (draft.attachments) target.attachments = draft.attachments;
      target.edit++;
      const savedDraft = target.persist?.();
      target.attachmentsMemoryOnly = Boolean(draft.attachments) && savedDraft !== "saved";
      if (savedDraft !== "saved") target.feedback = "The draft is available in this tab until reload.";
      target.changes.dispatchEvent(new Event("change"));
    }
    const directory = flowCatalog;
    const connection = connectionRead?.signal;
    if (directory && connection) void directory.readFlows(owner, connection).then((edition) => {
      if (api !== source || connection.aborted) return;
      directory.store(edition);
      if (selectedEdition?.view.session_id === owner.session_id) selectedEdition = edition;
      renderFlows(source, edition);
    }).catch(() => { /* The saved panel already has a verified conversation. */ });
    return true;
  }

  async function bringQuestionAnswer(question: QuestionWindow, key: string, questionRead: AbortController,
    originalRead: AbortController, answer: string, available: () => boolean): Promise<string | undefined> {
    const source = flowKey(question.flow);
    const [targetRead, target] = [...activityTargets].find(([read, view]) => read === originalRead && flowKey(view.flow) === source)
      ?? [...activityTargets].find(([, view]) => flowKey(view.flow) === source) ?? [];
    if (!targetRead || !target) return "Open the source Flow to bring this answer.";
    const selection = panelSelection;
    const signal = AbortSignal.any([questionRead.signal, targetRead.signal]);
    const current = () => !signal.aborted && available() && !openingSession && activeWindow === key && selection === panelSelection;
    if (!current()) return;
    const destination = await target.draft.check();
    if (!current()) return;
    if (!destination) return "Could not read the source branch.";
    const head = destination.head;
    if (head.branch !== question.source.head.branch) {
      if (!await confirmQuestionBring(target.label, head.branch, answer, signal) || !current()) return;
      const verified = await target.draft.check();
      if (!current()) return;
      if (!sameHistoryHead(verified?.head, head)) return "Source branch changed again. Review the target again.";
    }
    const issue = target.draft.appendQuote(head, answer);
    if (issue) return issue;
    if (targetRead === conversationRead) windows.main();
    else windows.focus(flowKey(target.flow));
    showConversationTarget(target.pane);
    composer.querySelector<HTMLTextAreaElement>("textarea[aria-label='Message']")?.focus({ preventScroll: true });
  }

  function installConversation(flow: FlowClient, pane: HTMLElement, read: AbortController,
    notify: (text: string, loading?: boolean, failed?: boolean) => void, label: string, verified?: () => void,
    view?: Dtos["SessionView"], seed?: ConversationSeed): void {
    const key = flowKey(flow);
    const cached = views.get(key);
    views.delete(key);
    read.signal.addEventListener("abort", () => { activityTargets.delete(read); }, { once: true });
    showConversation(flow, pane, notify, read.signal, {
      ...(seed === undefined ? (cached === undefined ? {} : { cached }) : { seed }),
      verified: (draftView) => {
        if (read.signal.aborted) return;
        activityTargets.set(read, { flow, label, pane, draft: draftView, ...(view === undefined ? {} : { view }) });
        verified?.();
        syncActivity();
      },
      executionChanged: () => {
        activity.invalidate(flow);
        approvals.invalidate(flow);
        overview.invalidate(flow);
      },
      live: draftsSupported,
      artifacts: artifactsSupported,
      images: imagesSupported,
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
      ...(questionsSupported ? { askQuestion: (page) => openQuestionWindow(flow, page, read) } : {}),
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

  async function confirmQuestionBring(label: string, branch: string, answer: string, signal: AbortSignal): Promise<boolean> {
    if (signal.aborted) return false;
    const { node, form } = dialog("Source branch changed");
    node.setAttribute("aria-label", "Source branch changed");
    const destination = element("p", "dialog-copy", `Add this answer as a quote in ${label} · ${branch.slice(0, 8)}?`);
    destination.title = branch;
    const cancel = button("Cancel", "Cancel", () => node.close());
    cancel.autofocus = true;
    const approve = element("button", "primary", "Add quote");
    approve.type = "submit";
    const actions = element("div", "dialog-actions");
    actions.append(cancel, approve);
    form.append(destination, element("blockquote", "dialog-copy question-quote-preview", answer), actions);
    form.addEventListener("submit", (event) => { event.preventDefault(); node.close("approved"); });
    const abort = () => node.close();
    signal.addEventListener("abort", abort, { once: true });
    try {
      return await new Promise<boolean>((resolve) => {
        node.addEventListener("close", () => resolve(!signal.aborted && node.returnValue === "approved"), { once: true });
        node.showModal();
      });
    } finally {
      signal.removeEventListener("abort", abort);
      node.remove();
    }
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
    retireProjectPreviews();
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
      artifactsSupported = conversationSupported && available("flow.conversation_artifact");
      imagesSupported = conversationSupported && available("flow.conversation_image");
      directorySupported = available("project.directory");
      if (directorySupported) projectDirectory.bind(source, connection.signal);
      projectPreviewSupported = conversationSupported && available("flow.preview");
      const workspaceSupported = projectsSupported && conversationSupported && ["project.workspaceRoots", "project.workspaceFolders",
        "project.openWorkspace", "project.workspaceOpenStatus", "session.open", "session.flows", "flow.events"].every(available);
      newProjectButton.hidden = !workspaceSupported;
      if (workspaceSupported) workspacePicker.bind({ api: source, scope: draftScope, signal: connection.signal });
      questionsSupported = conversationSupported && questionMethods.every(available);
      questionControls = { stop: questionsSupported && questionStopMethods.every(available),
        refresh: questionsSupported && questionRefreshMethods.every(available), keep: questionsSupported && questionKeepMethods.every(available),
        models: questionsSupported && questionModelMethods.every(available), live: questionsSupported && available(questionLiveMethod) };
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
    retireProjectPreviews();
    workspacePicker.unbind(!reason);
    newProjectButton.hidden = true;
    settingsRequest?.abort();
    settingsRequest = undefined;
    coAuthorValue = undefined;
    settings.node.close();
    settingsButton.hidden = true;
    overviewSupported = false;
    overview.select(undefined);
    activitySupported = false;
    draftsSupported = false;
    artifactsSupported = false;
    imagesSupported = false;
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
    questionWindows.clear();
    questionsSupported = false;
    questionControls = { stop: false, refresh: false, keep: false, models: false, live: false };
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
    projectDirectory.close();
    directorySupported = false;
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
  window.addEventListener("pagehide", () => {
    for (const question of questionWindows.values()) question.flushDraft();
  }, listener);
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
  newProjectButton.addEventListener("click", () => {
    workspacePicker.show(newProjectButton);
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
    retireProjectPreviews();
    projectDirectory.close();
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
    void unmount(workspacePicker);
    if (historyPanel) void unmount(historyPanel);
    historyDialog.remove();
    views.clear();
    auth.node.remove();
    createDialog.remove();
    settings.node.remove();
  };
}
