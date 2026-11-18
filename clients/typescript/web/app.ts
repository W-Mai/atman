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
let selected: Dtos["SessionView"] | undefined;
let selectionRead: AbortController | undefined;
let conversationRead: AbortController | undefined;
let conversationSupported = false;
const drafts = new Map<string, Draft>();
let sessionRows: Dtos["ListSessionsResponse"] = [];

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

function empty(title: string, copy: string, action?: HTMLButtonElement): void {
  const block = element("section", "empty");
  block.append(element("h2", "", title), element("p", "muted", copy));
  if (action !== undefined) {
    const actions = element("div", "empty-actions");
    actions.append(action);
    block.append(actions);
  }
  content.replaceChildren(block);
}

function closeConversation(): void {
  conversationRead?.abort();
  conversationRead = undefined;
  composer.replaceChildren();
}

function renderSessions(): void {
  sessions.replaceChildren(
    ...sessionRows.map((row) => {
      const node = button(
        "",
        row.title ?? row.id,
        () => {
          setSidebar(false);
          void selectSession(row.id);
        },
        "session-row",
      );
      node.setAttribute(
        "aria-current",
        String(row.id === selected?.session_id),
      );
      const copy = element(
        "span",
        "session-title",
        row.title || row.id.slice(0, 8),
      );
      copy.append(
        element(
          "span",
          "session-meta",
          row.project_root?.split(/[/\\]/).filter(Boolean).at(-1) || row.status,
        ),
      );
      node.append(element("span", "session-marker"), copy);
      return node;
    }),
  );
  if (sessionRows.length === 0)
    sessions.append(element("p", "sidebar-message", "No Sessions"));
}

async function loadSessions(): Promise<void> {
  const source = api;
  if (source === undefined) return;
  listingRead?.abort();
  const read = new AbortController();
  listingRead = read;
  const request = ++listingRevision;
  sidebar.setAttribute("aria-busy", "true");
  try {
    const query = search.value.trim();
    const rows = await source.listSessions(
      { limit: 100, ...(query ? { search: query } : {}) },
      { signal: read.signal },
    );
    if (api !== source || request !== listingRevision) return;
    sessionRows = rows;
    renderSessions();
  } catch (error) {
    if (api === source && request === listingRevision && !read.signal.aborted)
      setStatus(message(error), false, true);
  } finally {
    if (listingRead === read) sidebar.removeAttribute("aria-busy");
  }
}

async function selectSession(id: string): Promise<void> {
  const source = api;
  if (source === undefined) return;
  selectionRead?.abort();
  const read = new AbortController();
  selectionRead = read;
  const request = ++revision;
  selected = undefined;
  closeConversation();
  tabs.replaceChildren();
  empty("Opening Session", "Preparing the selected target.");
  setStatus("Opening Session", true);
  try {
    const target = source.session(id);
    const opened = await target.open({ signal: read.signal });
    if (api !== source || request !== revision) return;
    selected = opened.result;
    targetTitle.textContent = selected.title || id.slice(0, 8);
    renderSessions();
    const flows = await target.flows({ limit: 50 }, { signal: read.signal });
    if (api !== source || request !== revision) return;
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
    if (rows.length === 0) {
      empty(
        "Start here",
        "Create the canonical conversation in this Session.",
        button(
          "Start conversation",
          "Start conversation",
          () => {
            void bootstrap();
          },
          "primary",
        ),
      );
    } else {
      const first =
        rows.find((row) => row.flow.instance_id === selected?.default_flow) ??
        rows[0]!;
      selectFlow(target.flow(first.flow.instance_id));
    }
    setStatus("");
  } catch (error) {
    if (api === source && request === revision && !read.signal.aborted)
      setStatus(message(error), false, true);
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

async function bootstrap(): Promise<void> {
  const source = api;
  const original = selected;
  if (source === undefined || original === undefined) return;
  const request = revision;
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
    "Use the auth_token from this host’s daemon.toml. Credentials stay in memory for this connection.",
  ),
  credential.label,
  auth.error,
  connectButton,
);
auth.form.addEventListener("submit", (event) => {
  event.preventDefault();
  void connect();
});

async function connect(): Promise<void> {
  const attempt = ++connectionRevision;
  connectButton.disabled = true;
  auth.error.textContent = "";
  try {
    const transport = new HttpClient({
      endpoint: `${location.origin}/`,
      token: credential.input.value,
      contract,
    });
    const source = new AtmanClient(transport);
    const negotiated = await source.capabilities();
    if (attempt !== connectionRevision) return;
    api = source;
    conversationSupported =
      negotiated.methods["flow.conversation"]?.revision === 1;
    credential.input.value = "";
    connectionState.textContent = "Connected";
    auth.node.close();
    await loadSessions();
  } catch (error) {
    if (attempt === connectionRevision) auth.error.textContent = message(error);
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
      await loadSessions();
      if (original.selection === revision)
        await selectSession(original.request.session_id);
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
  api = undefined;
  connectButton.disabled = false;
  revision++;
  listingRevision++;
  if (searchTimer !== undefined) clearTimeout(searchTimer);
  listingRead?.abort();
  search.value = "";
  setSidebar(false);
  selectionRead?.abort();
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
  sessionRows = [];
  sessions.replaceChildren();
  tabs.replaceChildren();
  targetTitle.textContent = "No Session selected";
  connectionState.textContent = "Disconnected";
  setStatus("");
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
  auth.node.showModal();
}

disconnect();
