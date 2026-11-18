import {
  AtmanClient,
  ClientError,
  HttpClient,
  contract,
} from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import { showConversation } from "./conversation.ts";
import type { Draft } from "./conversation.ts";

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

const shell = element("div", "shell");
const header = element("header", "topbar");
const brand = element("div", "brand");
const sidebarToggle = button(
  "☰",
  "Toggle Sessions",
  () => setSidebar(!sidebarOpen),
  "icon sidebar-toggle",
);
sidebarToggle.setAttribute("aria-controls", "session-sidebar");
sidebarToggle.setAttribute("aria-expanded", "false");
const logo = element("img", "brand-logo");
logo.src = "/assets/ATMAN-LOGO.png";
logo.alt = "";
logo.width = 32;
logo.height = 32;
brand.append(sidebarToggle, logo, element("h1", "", "atman"));
const connection = element("div", "connection");
const connectionState = element("span", "connection-state", "Disconnected");
connection.append(
  connectionState,
  button("↪", "Disconnect", disconnect, "icon"),
);
header.append(brand, connection);
const sidebar = element("nav", "sidebar");
sidebar.id = "session-sidebar";
sidebar.setAttribute("aria-label", "Sessions");
const tools = element("div", "sidebar-tools");
tools.append(
  element("span", "sidebar-title", "Sessions"),
  button("+", "Create Session", () => createDialog.showModal(), "icon"),
  button(
    "↻",
    "Refresh Sessions",
    () => {
      void loadSessions();
    },
    "icon",
  ),
);
const sessions = element("div");
const search = element("input", "session-search");
search.type = "search";
search.placeholder = "Find Session";
search.setAttribute("aria-label", "Find Session");
sidebar.append(tools, search, sessions);
const workspace = element("main", "workspace");
const workspaceBar = element("div", "workspace-bar");
const targetTitle = element("div", "target-title muted", "No Session selected");
const tabs = element("div", "flow-tabs");
tabs.setAttribute("aria-label", "Flows");
workspaceBar.append(targetTitle, tabs);
const content = element("div", "content");
const composer = element("div", "composer-slot");
const status = element("div", "status-line");
status.setAttribute("role", "status");
status.setAttribute("aria-live", "polite");
workspace.append(workspaceBar, content, composer, status);
shell.append(header, sidebar, workspace);
document.querySelector("#app")!.append(shell);

let api: AtmanClient | undefined;
let connectionRevision = 0;
let revision = 0;
let listingRevision = 0;
let searchTimer: ReturnType<typeof setTimeout> | undefined;
let listingRead: AbortController | undefined;
let sidebarOpen = false;
let requestedSession: string | undefined;
let selected: Dtos["SessionView"] | undefined;
let selectionRead: AbortController | undefined;
let conversationRead: AbortController | undefined;
let conversationSupported = false;
const drafts = new Map<string, Draft>();
let catalog: { query: string; rows: Dtos["ListSessionsResponse"] } | undefined;
let catalogError: string | undefined;

const browserKeys = { token: "atman.web.token", session: "atman.web.session" };

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

function setSidebar(open: boolean): void {
  sidebarOpen = open;
  shell.classList.toggle("sidebar-open", open);
  sidebarToggle.setAttribute("aria-expanded", String(open));
}

search.addEventListener("input", () => {
  listingRead?.abort();
  if (searchTimer !== undefined) clearTimeout(searchTimer);
  searchTimer = setTimeout(() => {
    void loadSessions();
  }, 200);
});

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
  composer.replaceChildren();
}

function sessionRow(row: Dtos["ListSessionsResponse"][number]): HTMLButtonElement {
  const label = row.title || row.id.slice(0, 8);
  const node = button("", label, () => {
    setSidebar(false);
    if (row.persistent == null) {
      showLegacySession(row.id, label);
      return;
    }
    void selectSession(row.id);
  }, "session-row");
  node.setAttribute("aria-current", String(row.id === requestedSession));
  const copy = element("span", "session-title", label);
  copy.append(
    element(
      "span",
      "session-meta",
      row.project_root?.split(/[/\\]/).filter(Boolean).at(-1) || row.status,
    ),
  );
  node.append(element("span", "session-marker"), copy);
  return node;
}

function renderSessions(): void {
  sessions.replaceChildren(...(catalog?.rows ?? []).map(sessionRow));
  if (catalogError !== undefined) {
    sessions.append(element("p", "sidebar-message error", catalogError));
  } else if (catalog?.rows.length === 0) {
    sessions.append(
      element(
        "p",
        "sidebar-message",
        catalog.query ? "No matches" : "No Sessions",
      ),
    );
  }
}

function renderWorkspaceLanding(): void {
  if (requestedSession !== undefined) return;

  const create = button(
    "New Session",
    "New Session",
    () => createDialog.showModal(),
    "primary",
  );
  if (catalogError !== undefined) {
    empty(
      "Couldn’t load Sessions",
      catalogError,
      button("Retry", "Retry loading Sessions", () => void loadSessions()),
      create,
    );
  } else if (catalog === undefined) {
    empty("Loading Sessions…", "");
  } else if (catalog.rows.length > 0) {
    const recent = element("div", "recent-sessions");
    recent.setAttribute("aria-label", "Recent Sessions");
    const resumable = catalog.rows.filter((row) => row.persistent != null);
    recent.append(...(resumable.length > 0 ? resumable : catalog.rows).slice(0, 5).map(sessionRow));
    const landing = empty(
      catalog.query ? "Matching Sessions" : "Recent Sessions",
      "",
      button("Browse Sessions", "Browse Sessions", () => {
        setSidebar(true);
        sessions.querySelector("button")?.focus();
      }),
      button("New Session", "New Session", () => createDialog.showModal()),
    );
    landing.insertBefore(recent, landing.lastChild);
  } else if (catalog.query) {
    empty(
      "No matching Sessions",
      "Try a different search or start a new Session.",
      create,
      button("Clear search", "Clear Session search", () => {
        search.value = "";
        void loadSessions();
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
  const request = ++listingRevision;
  catalogError = undefined;
  sidebar.setAttribute("aria-busy", "true");
  renderWorkspaceLanding();
  try {
    const query = search.value.trim();
    const rows = await source.listSessions(
      { limit: 100, ...(query ? { search: query } : {}) },
      { signal: read.signal },
    );
    if (api !== source || request !== listingRevision || read.signal.aborted)
      return;
    catalog = { query, rows };
  } catch (error) {
    if (api === source && request === listingRevision && !read.signal.aborted)
      catalogError = message(error);
  } finally {
    if (listingRead === read) {
      listingRead = undefined;
      sidebar.removeAttribute("aria-busy");
      renderSessions();
      renderWorkspaceLanding();
    }
  }
}

function showLegacySession(id: string, label: string): void {
  selectionRead?.abort();
  revision++;
  requestedSession = id;
  selected = undefined;
  closeConversation();
  tabs.replaceChildren();
  targetTitle.textContent = label;
  setStatus("");
  renderSessions();
  const block = empty("Open this Session in the terminal", "Resume it in atman to migrate its history.");
  block.append(element("code", "", `atman --continue ${id}`));
}

async function selectSession(id: string, prepare = false): Promise<void> {
  const source = api;
  if (source === undefined) return;
  selectionRead?.abort();
  const read = new AbortController();
  selectionRead = read;
  const request = ++revision;
  requestedSession = id;
  selected = undefined;
  closeConversation();
  tabs.replaceChildren();
  renderSessions();
  empty("Opening Session", "Preparing the selected target.");
  setStatus("Opening Session", true);
  try {
    const target = source.session(id);
    const opened = await target.open({ signal: read.signal });
    if (api !== source || request !== revision) return;
    selected = opened.result;
    targetTitle.textContent = selected.title || id.slice(0, 8);
    const flows = await target.flows({ limit: 50 }, { signal: read.signal });
    if (api !== source || request !== revision) return;
    remember("session", id);
    const rows = flows.result.instances;
    for (const row of rows) {
      const node = button(
        row.entry || row.flow.instance_id.slice(0, 8),
        row.entry || row.flow.instance_id,
        () => selectFlow(target.flow(row.flow.instance_id)),
        "flow-tab",
      );
      node.dataset.instance = row.flow.instance_id;
      tabs.append(node);
    }
    setStatus("");
    if (rows.length === 0) {
      const start = button(
        "Start conversation",
        "Start conversation",
        () => void bootstrap(start),
        "primary",
      );
      empty(
        "Start here",
        "",
        start,
      );
      if (prepare) await bootstrap(start);
    } else {
      const first =
        rows.find((row) => row.flow.instance_id === selected?.default_flow) ??
        rows[0]!;
      selectFlow(target.flow(first.flow.instance_id));
    }
  } catch (error) {
    if (api !== source || request !== revision || read.signal.aborted) return;
    setStatus("");
    empty(
      "Couldn’t open Session",
      message(error),
      button(
        "Retry",
        "Retry opening Session",
        () => void selectSession(id),
        "primary",
      ),
    );
  }
}

function selectFlow(flow: FlowClient): void {
  closeConversation();
  for (const node of tabs.querySelectorAll<HTMLButtonElement>("button"))
    node.setAttribute(
      "aria-pressed",
      String(node.dataset.instance === flow.address.instance_id),
    );
  if (!conversationSupported) {
    empty(
      "Flow selected",
      "This host does not provide browser conversation reads.",
    );
    return;
  }
  const key = `${flow.address.session_id}/${flow.address.instance_id}`;
  let draft = drafts.get(key);
  if (!draft) {
    draft = { text: "", edit: 0, changes: new EventTarget() };
    drafts.set(key, draft);
  }
  conversationRead = new AbortController();
  showConversation(
    flow,
    content,
    composer,
    draft,
    setStatus,
    conversationRead.signal,
  );
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
        unauthorized = response.status === 401;
        return response;
      },
    });
    const source = new AtmanClient(transport);
    const negotiated = await source.capabilities();
    if (attempt !== connectionRevision) return;
    api = source;
    remember("token", token);
    conversationSupported =
      negotiated.methods["flow.conversation"]?.revision === 1;
    credential.input.value = "";
    connectionState.textContent = "Connected";
    auth.node.close();
    void loadSessions();
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
      void loadSessions();
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

function disconnect(): void {
  connectionRevision++;
  remember("token");
  remember("session");
  api = undefined;
  credential.input.value = "";
  auth.error.textContent = "";
  connectButton.disabled = false;
  revision++;
  listingRevision++;
  if (searchTimer !== undefined) clearTimeout(searchTimer);
  listingRead?.abort();
  listingRead = undefined;
  sidebar.removeAttribute("aria-busy");
  search.value = "";
  setSidebar(false);
  selectionRead?.abort();
  requestedSession = undefined;
  selected = undefined;
  closeConversation();
  drafts.clear();
  conversationSupported = false;
  pendingCreate = undefined;
  title.input.readOnly = false;
  directory.input.readOnly = false;
  createButton.disabled = false;
  createButton.textContent = "Create";
  createDialog.close();
  catalog = undefined;
  catalogError = undefined;
  sessions.replaceChildren();
  tabs.replaceChildren();
  targetTitle.textContent = "No Session selected";
  connectionState.textContent = "Disconnected";
  setStatus("");
  renderDisconnected();
  if (!auth.node.open) auth.node.showModal();
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

const savedToken = saved("token");
if (savedToken === undefined) {
  renderDisconnected();
  auth.node.showModal();
} else {
  void connect(savedToken);
}
