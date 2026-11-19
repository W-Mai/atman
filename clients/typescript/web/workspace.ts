import {
  AtmanClient,
  ClientError,
  HttpClient,
  canonicalUuid,
  contract,
} from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import { showConversation } from "./conversation.ts";
import { flowWindows } from "./flow-windows.ts";
import { sharedConsole } from "./shared-console.ts";
import type { ConversationMemory, Draft } from "./conversation.ts";
import { SessionCatalog, initialFlow, mergeFlowPages } from "./session-catalog.ts";
import type { SessionEdition } from "./session-catalog.ts";
import type { WorkbenchElements } from "./shell-elements.ts";
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
    newSessionButton,
    refreshButton,
  } = elements;

  const input = sharedConsole(composer);
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
  const windowReads = new Map<string, AbortController>();
  let activeWindow: string | undefined;
  const windows = flowWindows(content.parentElement!, content, status, composer, (key) => {
    activeWindow = key;
    input.activate(key === undefined ? conversationRead : windowReads.get(key));
  });
  const drafts = new Map<string, Draft>();
  const draftArchive = new TabDrafts(tabStorage());
  let draftScope: string | undefined;
  let originalCredential: string | undefined;
  const lifetime = new AbortController();
  type SessionRow = Dtos["ListSessionsResponse"][number];
  type ProjectPage = Dtos["ProjectListPage"];
  type ProjectGroup = { root: string; label: string; pinned: boolean; sessions: SessionRow[] };
  let sessionRows: readonly SessionRow[] | undefined;
  let projectPage: ProjectPage | undefined;
  let projectsSupported = false;
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
      ...(restored?.pending === undefined ? {} : { pending: restored.pending }),
      changes: new EventTarget(),
    };
    if (binding) {
      draft.persist = () => {
        draftArchive.save(binding, {
          text: draft.text,
          edit: draft.edit,
          ...(draft.pending === undefined ? {} : { pending: draft.pending }),
        });
      };
    }
    drafts.set(key, draft);
    return draft;
  }

  function setOpening(opening: boolean): void {
    openingSession = opening;
    composer.inert = opening;
    status.classList.toggle("in-place", opening && activeFlow !== undefined);
  }

  function setSidebar(open: boolean): void {
    sidebarOpen = open;
    shell.classList.toggle("sidebar-open", open);
    updateSidebarToggle();
  }

  function updateSidebarToggle(): void {
    sidebarToggle.setAttribute("aria-expanded", String(narrowProjects.matches ? sidebarOpen : desktopSidebarOpen));
  }

  function setDesktopSidebar(open: boolean): void {
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

  function closeConversation(): void {
    conversationRead?.abort();
    conversationRead = undefined;
    activeFlow = undefined;
    if (activeWindow === undefined) input.activate();
  }

  function isCurrentSession(row: SessionRow): boolean {
    return (
      row.id === requestedSession && (row.persistent == null) === requestedLegacy
    );
  }

  function sessionRow(row: SessionRow): HTMLButtonElement {
    const label = row.title || row.id.slice(0, 8);
    const node = button("", label, () => {
      setSidebar(false);
      sidebarProjects.set(row.project_root ?? "", true);
      if (row.persistent == null) {
        showLegacySession(row.id, label);
        return;
      }
      void selectSession(row.id);
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
    return node;
  }

  function groupProjects(): ProjectGroup[] {
    const query = search.value.trim().toLowerCase();
    const groups = new Map<string, ProjectGroup>();
    for (const project of projectPage?.projects ?? []) {
      groups.set(project.project_root, {
        root: project.project_root,
        label: project.display_name,
        pinned: project.pinned,
        sessions: [],
      });
    }
    for (const row of sessionRows ?? []) {
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
        if (project.sessions.length) list.append(...project.sessions.map(sessionRow));
        else if (sessionRows !== undefined) list.append(element("p", "sidebar-message", "No Sessions"));
      };
      if (group.open) showRows();
      summary.addEventListener("click", () => {
        if (!searching) expanded.set(project.root, !group.open);
        if (!group.open) showRows();
      });
      group.append(summary, list);
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
    more.disabled = projectRead !== undefined;
    return [more];
  }

  function renderSessions(): void {
    const focused = document.activeElement as HTMLElement | null;
    const focus = sessions.contains(focused) ? focused?.dataset : undefined;
    const onMore = sessions.contains(focused) && focused?.matches(".directory-more");
    sessions.replaceChildren();
    const projects = groupProjects();
    const searching = Boolean(search.value.trim());
    if (projects.length) {
      sessions.append(
        projectTree(projects, sidebarProjects, searching),
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
    const target = focus?.session !== undefined
      ? [...sessions.querySelectorAll<HTMLElement>("[data-session]")].find((row) => row.dataset.session === focus.session)
      : focus?.project !== undefined
        ? [...sessions.querySelectorAll<HTMLElement>("[data-project]")].find((row) => row.dataset.project === focus.project)
        : onMore ? sessions.querySelector<HTMLElement>(".directory-more") ?? refreshButton : undefined;
    target?.focus({ preventScroll: true });
  }

  function renderWorkspaceLanding(): void {
    if (requestedSession !== undefined) return;

    const create = button(
      "New Session",
      "New Session",
      () => createDialog.showModal(),
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
        button("New Session", "New Session", () => createDialog.showModal()),
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
    if (!source || !projectsSupported || (more && (projectRead || projectPage?.after == null))) return;
    projectRead?.abort();
    const read = new AbortController();
    const original = projectPage;
    const after = more ? original?.after : undefined;
    projectRead = read;
    projectError = undefined;
    try {
      const page = await source.listProjects(after == null ? {} : { after }, { signal: read.signal });
      if (api !== source || projectRead !== read) return;
      projectPage = after != null && original ? { ...page, projects: [...original.projects, ...page.projects] } : page;
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

  function refreshDirectory(): void {
    void loadProjects();
    void loadSessions();
  }

  function showLegacySession(id: string, label: string): void {
    revision++;
    setOpening(false);
    requestedSession = id;
    requestedLegacy = true;
    selected = undefined;
    selectedEdition = undefined;
    closeConversation();
    tabs.replaceChildren();
    targetTitle.textContent = label;
    setStatus("");
    renderSessions();
    const block = empty("Open this Session in the terminal", "Resume it in atman to migrate its history.");
    block.append(element("code", "", `atman --continue ${id}`));
  }

  function renderFlows(source: AtmanClient, edition: SessionEdition): void {
    const { view, flows } = edition;
    const target = source.session(view.session_id);
    const focused = tabs.contains(document.activeElement)
      ? (document.activeElement as HTMLElement).dataset.instance
      : undefined;
    const flowButton = (id: string, label: string) => {
      const node = button(label, label, () => selectFlow(target.flow(id)), "flow-tab");
      node.dataset.instance = id;
      node.setAttribute("aria-pressed", String(activeFlow?.address.instance_id === id));
      if (!conversationSupported || activeFlow?.address.instance_id === id) return node;
      const row = element("span", "flow-row");
      const panel = button("▣", `Open ${label} in panel`, () => openFlowPanel(target.flow(id), label), "flow-panel-launch");
      row.append(node, panel);
      return row;
    };
    const buttons = flows.instances.map((row) => {
      const id = row.flow.instance_id;
      return flowButton(id, row.entry || id.slice(0, 8));
    });
    const omitted = new Set([view.default_flow, activeFlow?.address.instance_id]);
    for (const id of omitted) {
      if (id != null && !flows.instances.some((row) => row.flow.instance_id === id)) {
        buttons.unshift(flowButton(id, id === view.default_flow ? "Default Flow" : id.slice(0, 8)));
      }
    }
    if (flows.next != null) {
      buttons.push(button("…", "Load more Flows", () => void loadFlows(source, edition), "flow-tab flow-more"));
    }
    tabs.replaceChildren(...buttons);
    if (focused !== undefined) {
      [...tabs.querySelectorAll<HTMLButtonElement>("button[data-instance]")].find((node) => node.dataset.instance === focused)?.focus({ preventScroll: true });
    }
  }

  async function loadFlows(source: AtmanClient, edition: SessionEdition): Promise<void> {
    const { view, flows } = edition;
    const before = flows.next;
    if (before == null || loadingFlowPages.has(edition) || selectedEdition !== edition || openingSession) return;
    const signal = connectionRead?.signal;
    if (!signal || signal.aborted) return;
    loadingFlowPages.add(edition);
    const more = tabs.querySelector<HTMLButtonElement>(".flow-more");
    if (more) more.disabled = true;
    try {
      const { result: page } = await source.session(view.session_id).flows({ before, limit: 50 }, { signal });
      if (api !== source || selectedEdition !== edition || openingSession) return;
      const merged = mergeFlowPages(flows, page);
      if (!merged) {
        await selectSession(view.session_id);
        return;
      }
      const next = { view, flows: merged };
      selectedEdition = next;
      flowCatalog?.store(next);
      renderFlows(source, next);
      if (more && document.activeElement === document.body) {
        tabs.querySelector<HTMLButtonElement>(".flow-more")?.focus({ preventScroll: true });
      }
    } catch (error) {
      if (api === source && selectedEdition === edition) setStatus(message(error), false, true);
    } finally {
      loadingFlowPages.delete(edition);
      if (more?.isConnected) more.disabled = false;
    }
  }

  function installSession(source: AtmanClient, edition: SessionEdition): HTMLButtonElement | undefined {
    const { view, flows } = edition;
    const id = view.session_id;
    const sameView = selected?.session_id === id && activeFlow !== undefined;
    selected = view;
    selectedEdition = edition;
    targetTitle.textContent = view.title || id.slice(0, 8);
    remember("session", id);
    setOpening(false);
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
          renderFlows(source, edition);
          return start;
        }
        empty("Select a Flow", "Open Overview to choose a Flow.");
      }
    }
    renderFlows(source, edition);
  }

  async function selectSession(id: string, prepare = false): Promise<void> {
    const source = api;
    const directory = flowCatalog;
    if (!source || !directory) return;
    windows.main();
    const request = ++revision;
    const displayed = selected;
    const cached = prepare ? undefined : directory.cached(id);
    requestedSession = id;
    requestedLegacy = false;
    renderSessions();
    if (cached) {
      installSession(source, cached);
    } else {
      setOpening(true);
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
    windows.main();
    windows.close(flowKey(flow));
    for (const node of tabs.querySelectorAll<HTMLButtonElement>("button[data-instance]")) {
      node.setAttribute("aria-pressed", String(node.dataset.instance === flow.address.instance_id));
    }
    if (flow.address.session_id === activeFlow?.address.session_id &&
        flow.address.instance_id === activeFlow.address.instance_id) return;
    closeConversation();
    if (!conversationSupported) {
      empty("Flow selected", "This host does not provide browser conversation reads.");
      return;
    }
    const read = new AbortController();
    conversationRead = read;
    activeFlow = flow;
    input.activate(read);
    installConversation(flow, content, read, (text, loading, failed) => {
      if (!openingSession && conversationRead === read) setStatus(text, loading, failed);
    }, () => rememberFlow(flow));
    if (api && selectedEdition) renderFlows(api, selectedEdition);
  }

  function flowKey(flow: FlowClient): string {
    return flow.address.session_id + "/" + flow.address.instance_id;
  }

  function openFlowPanel(flow: FlowClient, label: string): void {
    if (openingSession || !conversationSupported || (activeFlow && flowKey(activeFlow) === flowKey(flow))) return;
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
      });
      return () => {
        read.abort();
        windowReads.delete(key);
      };
    });
    if (!opened) setStatus("Close a Flow panel to open another.", false, true);
  }

  function installConversation(flow: FlowClient, pane: HTMLElement, read: AbortController,
    notify: (text: string, loading?: boolean, failed?: boolean) => void, verified?: () => void): void {
    const key = flowKey(flow);
    const cached = views.get(key);
    views.delete(key);
    showConversation(flow, pane, notify, read.signal, {
      ...(cached === undefined ? {} : { cached }),
      ...(verified === undefined ? {} : { verified }),
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
    closeConversation();
    windows.reset();
    draftScope = undefined;
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
  narrowProjects.addEventListener("change", updateSidebarToggle, listener);
  updateSidebarToggle();
  sidebarToggle.addEventListener("click", () => {
    if (!narrowProjects.matches) setDesktopSidebar(!desktopSidebarOpen);
    else if (sidebarOpen) setSidebar(false);
    else focusProjectSearch();
  }, listener);
  disconnectButton.addEventListener("click", () => disconnect(), listener);
  newSessionButton.addEventListener("click", () => {
    if (api !== undefined) createDialog.showModal();
  }, listener);
  refreshButton.addEventListener("click", refreshDirectory, listener);
  shell.addEventListener("keydown", (event) => {
    if (auth.node.open || createDialog.open) return;
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
    revision++;
    api = undefined;
    lifetime.abort();
    listingRead?.abort();
    projectRead?.abort();
    connectionRead?.abort();
    closeConversation();
    windows.destroy();
    input.destroy();
    views.clear();
    auth.node.remove();
    createDialog.remove();
  };
}
