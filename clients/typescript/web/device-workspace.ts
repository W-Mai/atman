import { BrowserDeviceClient, canonicalUuid, ClientError, methods } from "../src/index.ts";
import type { BrowserDeviceSession, Dtos, PairedDeviceClient, SharedSession } from "../src/index.ts";
import { ApprovalActivity, type ApprovalTarget, type ApprovalView } from "./approval-activity.ts";
import { ApprovalReceipts } from "./approval-receipts.ts";
import { showConversation } from "./conversation.ts";
import type { ConversationDraftView, ConversationMemory } from "./conversation.ts";
import { mergeFlowPages } from "./session-catalog.ts";
import { sharedConsole } from "./shared-console.ts";
import type { WorkbenchElements } from "./shell-elements.ts";
import { ConversationDrafts } from "./tab-drafts.ts";

const renewalKey = "atman.device.renewal";
const accessMargin = 30_000;
const pairingChanged = "Pairing changed in another tab. Reconnect to open the new Session.";

function node<K extends keyof HTMLElementTagNameMap>(tag: K, style: string, text = ""): HTMLElementTagNameMap[K] {
  const result = document.createElement(tag);
  result.className = style;
  result.textContent = text;
  return result;
}

function action(label: string, style: string, run: () => void): HTMLButtonElement {
  const button = node("button", style, label);
  button.type = "button";
  button.addEventListener("click", run);
  return button;
}

function rejected(error: unknown): boolean {
  return error instanceof ClientError && error.status === 401;
}

function checkDevice(expected: BrowserDeviceSession | undefined, actual: BrowserDeviceSession): void {
  if (expected && (actual.device.id !== expected.device.id ||
      actual.device.scope.session_id !== expected.device.scope.session_id ||
      actual.device.scope.permission !== expected.device.scope.permission)) {
    throw new ClientError("source", pairingChanged);
  }
}

function tabFlow(key: string, write?: string): string | undefined {
  try {
    if (write) sessionStorage.setItem(key, write);
    else return canonicalUuid(sessionStorage.getItem(key) ?? "");
  } catch { /* Browser storage is optional. */ }
}

function tabStorage(): Storage | undefined {
  try { return window.sessionStorage; }
  catch { return undefined; }
}

type Binding = {
  grant: BrowserDeviceSession;
  info: SharedSession;
  client: PairedDeviceClient;
  key: string;
  approvals?: true;
};

export function bindDeviceWorkbench(elements: WorkbenchElements): () => void {
  const { shell, sidebarToggle, sidebar, sessions, search, targetTitle, tabs, content, composer, status,
    connectionState, disconnectButton, refreshButton, presentFlowMap, showConversationTarget,
    openApprovals, presentApprovals, bindApprovalActions } = elements;
  const life = new AbortController();
  const listener = { signal: life.signal };
  const auth = new BrowserDeviceClient({ origin: location.origin });
  const narrow = matchMedia("(width < 48rem)");
  const memory = new Map<string, ConversationMemory>();
  const drafts = new ConversationDrafts();
  let input: ReturnType<typeof sharedConsole> | undefined;
  let approvalTarget: ApprovalTarget | undefined;
  let approvalView: ApprovalView | undefined;
  const approvalChanges = new EventTarget();
  const approvals = new ApprovalActivity((view) => {
    approvalView = view;
    presentApprovals(view);
    input?.presentApprovals(view);
    approvalChanges.dispatchEvent(new Event("change"));
  });
  bindApprovalActions({
    refresh: () => approvals.refresh(),
    review: (view, selection) => approvals.review(view, selection),
    dismiss: (view) => approvals.dismiss(view),
    issue: (view, action) => approvals.issue(view, action),
    check: (view) => approvals.check(view),
    dismissPending: (view) => approvals.dismissPending(view),
  });
  let attempt: AbortController | undefined;
  let read: AbortController | undefined;
  let page: Dtos["FlowListPage"] | undefined;
  let binding: Binding | undefined;
  let active: string | undefined;
  let conversation: ConversationDraftView | undefined;
  let paging = false;
  let renewing: Promise<BrowserDeviceSession> | undefined;
  let foregroundCheck: Promise<void> | undefined;
  let pending: string | undefined;
  let invitation: string | undefined;

  if (location.hash.startsWith("#pair=")) {
    invitation = location.href;
    history.replaceState(history.state, "", location.pathname + location.search);
  }

  function renewId(value?: string): string | undefined {
    try {
      if (value !== undefined) {
        pending = value || undefined;
        if (pending) localStorage.setItem(renewalKey, pending);
        else localStorage.removeItem(renewalKey);
      } else {
        const stored = localStorage.getItem(renewalKey);
        return stored ? canonicalUuid(stored) : undefined;
      }
    } catch { /* Keep the pending request in this tab when storage is unavailable. */ }
    return pending;
  }

  function exclusive<T>(work: () => Promise<T>): Promise<T> {
    return navigator.locks?.request(renewalKey, listener, work) ?? work();
  }

  function grant(expected?: BrowserDeviceSession): Promise<BrowserDeviceSession> {
    if (renewing) return renewing.then((value) => { checkDevice(expected, value); return value; });
    const work = exclusive(async () => {
      const current = await auth.me(listener).catch((error: unknown) => {
        if (rejected(error)) return undefined;
        throw error;
      });
      if (current) checkDevice(expected, current);
      const previous = renewId();
      if (!previous && current && Date.parse(current.access_expires_at) > Date.now() + accessMargin) return current;
      const id = previous ?? crypto.randomUUID();
      renewId(id);
      try {
        let next = await auth.refresh(id, listener);
        checkDevice(expected ?? current, next);
        renewId("");
        if (Date.parse(next.access_expires_at) <= Date.now()) {
          const retry = crypto.randomUUID();
          renewId(retry);
          next = await auth.refresh(retry, listener);
          checkDevice(expected ?? current, next);
          renewId("");
        }
        return next;
      } catch (error) {
        if (rejected(error)) renewId("");
        throw error;
      }
    });
    renewing = work.finally(() => { renewing = undefined; });
    return renewing;
  }

  function feedback(text = "", busy = false, failed = false): void {
    status.classList.toggle("error", failed);
    status.replaceChildren(...(busy ? [node("span", "loading-mark")] : []), document.createTextNode(text));
  }

  function drawer(open: boolean): void {
    shell.classList.toggle("sidebar-open", narrow.matches && open);
    shell.classList.toggle("projects-collapsed", !narrow.matches && !open);
    sidebarToggle.setAttribute("aria-expanded", String(open));
  }

  function retire(): void {
    binding = undefined;
    const previous = read;
    read = undefined;
    previous?.abort();
    approvalTarget = undefined;
    approvals.disconnect();
    input?.activate();
    input?.destroy();
    input = undefined;
    drafts.clear();
    page = undefined;
    paging = false;
    active = undefined;
    conversation = undefined;
    memory.clear();
    sessions.replaceChildren();
    tabs.replaceChildren();
    presentFlowMap(undefined);
    disconnectButton.hidden = true;
    refreshButton.hidden = true;
  }

  function signedOut(reason?: string, retry = false): void {
    retire();
    connectionState.textContent = "Disconnected";
    targetTitle.textContent = "Shared Session";
    sidebarToggle.disabled = true;
    sidebar.removeAttribute("aria-busy");
    feedback();
    const box = node("section", "empty");
    box.append(node("h2", "", "Pair this browser"));
    const form = node("form", "dialog-form device-pair");
    const field = node("input", "");
    field.type = "password";
    field.inputMode = "url";
    field.autocomplete = "off";
    field.spellcheck = false;
    field.required = invitation === undefined;
    field.placeholder = "https://…";
    field.setAttribute("aria-label", "Invitation link");
    if (!invitation) form.append(field);
    const pair = node("button", "primary", "Pair device");
    form.append(pair);
    if (retry) form.append(action("Reconnect", "", () => void connect()));
    if (reason) {
      const error = node("p", "dialog-error", reason);
      error.setAttribute("role", "alert");
      form.prepend(error);
    }
    if (invitation) form.prepend(node("p", "", location.host));
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      pair.disabled = true;
      const link = invitation ?? field.value.trim();
      field.value = "";
      void connect(link);
    });
    box.append(form);
    content.replaceChildren(box);
  }

  function errorMessage(error: unknown): string {
    return error instanceof Error ? error.message : "Could not read the shared Session.";
  }

  function flowLabel(info: SharedSession, id: string, entry?: string | null): string {
    return id === info.default_flow ? "Main Flow" : entry || id.slice(0, 8);
  }

  function syncApprovals(): void {
    approvals.select(document.visibilityState === "hidden" ? undefined : approvalTarget);
  }

  function openFlow(id: string): void {
    const source = binding;
    if (!source) return;
    if (narrow.matches) drawer(false);
    if (active === id && read) { showConversationTarget(content); return; }
    read?.abort();
    const view = new AbortController();
    read = view;
    active = id;
    conversation = undefined;
    approvalTarget = undefined;
    syncApprovals();
    const flow = source.client.flow(id);
    const cached = memory.get(id);
    const console = input;
    feedback("Loading conversation", true);
    showConversation(flow, content, (message, loading, failed) => {
      if (read === view && binding === source) feedback(message, loading, failed);
    }, view.signal, {
      pauseWhenHidden: true,
      ...(cached ? { cached } : {}),
      active: () => read === view && binding === source,
      ...(console ? {
        deviceInput: true,
        draftFor: (page: Dtos["ConversationPage"]) => drafts.get(source.grant.device.id, flow.address, page),
        console: (snapshot, actions) => console.publish(view, snapshot, actions),
      } : {}),
      ...(source.approvals ? {
        executionChanged: () => approvals.invalidate(flow),
        approvals: {
          changes: approvalChanges,
          current: () => read === view && approvalTarget?.flow === flow ? approvalView : undefined,
          review: (observed, request, trigger) => {
            if (read !== view || binding !== source) return;
            const next = approvals.reviewTool(flow, observed, request);
            if (next) openApprovals(next, trigger);
          },
        },
      } : {}),
      verified: (verified) => {
        if (read !== view || binding !== source) return;
        conversation = verified;
        tabFlow(source.key, id);
        if (source.approvals) {
          const entry = page?.instances.find(({ flow }) => flow.instance_id === id);
          approvalTarget = { flow, label: flowLabel(source.info, id, entry?.entry) };
          syncApprovals();
        }
      },
      retain: (cachedPage) => {
        if (binding !== source) return;
        memory.delete(id);
        memory.set(id, cachedPage);
        const oldest = memory.keys().next().value;
        if (memory.size > 4 && oldest !== undefined) memory.delete(oldest);
      },
    });
    console?.activate(view);
    directory();
    showConversationTarget(content);
  }

  function directory(): void {
    if (!binding || !page) return;
    const { info } = binding;
    const entries = page.instances;
    const focused = tabs.contains(document.activeElement) ? (document.activeElement as HTMLElement).dataset.instance : undefined;
    const label = (row: Dtos["FlowListEntry"]) => flowLabel(info, row.flow.instance_id, row.entry);
    const choices = entries.map((row) => {
      const choice = action(label(row), "flow-tab", () => openFlow(row.flow.instance_id));
      choice.disabled = row.unavailable != null;
      choice.dataset.instance = row.flow.instance_id;
      choice.setAttribute("aria-pressed", String(active === row.flow.instance_id));
      return choice;
    });
    tabs.replaceChildren(...choices);
    if (focused) choices.find((choice) => choice.dataset.instance === focused)?.focus({ preventScroll: true });
    if (page.next) tabs.append(action("More Flows", "flow-tab", () => void loadDirectory(true)));
    presentFlowMap({
      session: info.session_id,
      ...(active ? { selected: { session_id: info.session_id, instance_id: active } } : {}),
      entries: entries.map((row) => ({ address: row.flow, title: label(row), archived: row.archived,
        unavailable: row.unavailable != null, ...(row.map ? { phase: row.map.phase, paused: row.map.paused,
          ...(row.map.parent ? { parent: row.map.parent } : {}) } : {}) })),
      hasRelations: entries.some((row) => row.map != null),
      refresh: () => { void loadDirectory(); },
      ...(page.next ? { more: () => loadDirectory(true) } : {}),
      onOpen: (address) => { if (binding?.info.session_id === address.session_id) openFlow(address.instance_id); },
    });
  }

  async function loadDirectory(older = false): Promise<void> {
    const current = binding;
    const previous = page;
    if (!current || paging || (older && !previous?.next)) return;
    paging = true;
    sidebar.setAttribute("aria-busy", "true");
    try {
      const { result } = await current.client.flows({ ...(older && previous?.next ? { before: previous.next } : {}), limit: 50 }, listener);
      if (binding !== current) return;
      page = older && previous ? mergeFlowPages(previous, result) : result;
      if (!page) {
        paging = false;
        return void loadDirectory();
      }
      if (!active) {
        const preferred = tabFlow(current.key);
        const open = preferred ?? current.info.default_flow ?? page.instances.find((item) => item.unavailable == null)?.flow.instance_id;
        if (open) openFlow(open);
        else content.replaceChildren(node("p", "empty", "This Session has no Flows yet."));
      }
      directory();
    } catch (error) {
      if (binding === current) feedback(errorMessage(error), false, true);
    } finally {
      if (binding === current) { paging = false; sidebar.removeAttribute("aria-busy"); }
    }
  }

  async function connect(link?: string): Promise<void> {
    attempt?.abort();
    const ticket = new AbortController();
    attempt = ticket;
    const signal = AbortSignal.any([life.signal, ticket.signal]);
    const valid = () => !signal.aborted && attempt === ticket;
    connectionState.textContent = "Connecting…";
    feedback("Connecting", true);
    try {
      const credentials = link ? await exclusive(async () => {
        const previous = await auth.me({ signal }).catch((error: unknown) => {
          if (rejected(error)) return undefined;
          throw error;
        });
        try {
          const paired = await auth.pair(link, { signal });
          renewId("");
          return paired;
        } catch (error) {
          if (error instanceof ClientError && error.outcome === "unknown" && !rejected(error)) {
            const installed = await auth.me({ signal }).catch(() => undefined);
            if (installed && installed.device.id !== previous?.device.id) return installed;
          }
          throw error;
        }
      }) : await grant();
      if (!valid()) return;
      if (link) invitation = undefined;
      const open = await auth.session(credentials, { signal });
      if (!valid()) return;
      let current: Binding;
      const client = auth.connect(credentials, async (url, init) => {
        if (binding !== current || init?.signal?.aborted) throw new DOMException("Device view retired", "AbortError");
        try {
          await foregroundCheck;
          if (Date.parse(current.grant.access_expires_at) <= Date.now() + accessMargin) {
            current.grant = await grant(current.grant);
          }
          if (binding !== current || init?.signal?.aborted) throw new DOMException("Device view retired", "AbortError");
          const response = await fetch(url, init);
          if (binding === current && response.status === 401) {
            signedOut("Device access expired or was revoked. Open a new invitation.");
          } else if (binding === current && response.status === 409) {
            signedOut(pairingChanged, true);
          }
          return response;
        } catch (error) {
          if (binding === current && (rejected(error) || (error instanceof ClientError && error.kind === "source"))) {
            signedOut(rejected(error) ? "Device access expired or was revoked. Open a new invitation." : errorMessage(error));
          }
          throw error;
        }
      });
      retire();
      current = { grant: credentials, info: open, client, key: `atman.device.flow.${credentials.device.id}.${open.session_id}` };
      binding = current;
      const permission = credentials.device.scope.permission;
      if (permission !== "read") {
        const capabilities = await client.capabilities({ signal });
        if (!valid() || binding !== current) return;
        const supports = (name: keyof typeof methods) => {
          const supported = capabilities.methods[name];
          const expected = methods[name];
          return supported?.revision === expected.revision && supported.request_schema === expected.request_schema &&
            supported.response_schema === expected.response_schema;
        };
        if ((permission === "send" || permission === "send_and_approve") && supports("flow.submit") && supports("flow.input_status")) {
          input = sharedConsole(composer, openApprovals);
        }
        if ((permission === "approve" || permission === "send_and_approve") && supports("flow.approval_queue") &&
            supports("flow.approval_apply") && supports("flow.approval_status")) {
          current.approvals = true;
          approvals.connect(new ApprovalReceipts(credentials.device.id, open.session_id, tabStorage()));
        }
      }
      disconnectButton.hidden = false;
      refreshButton.hidden = false;
      sidebarToggle.disabled = false;
      const title = open.title || "Shared Session";
      targetTitle.textContent = title;
      connectionState.textContent = input || current.approvals ? "Connected" : "Read only";
      const row = action("", "session-row", () => { if (narrow.matches) drawer(false); showConversationTarget(content); });
      row.append(node("span", "session-marker"), node("span", "session-title", title));
      row.setAttribute("aria-current", "true");
      sessions.replaceChildren(...(open.project ? [node("p", "sidebar-message", open.project)] : []), row);
      feedback();
      await loadDirectory();
    } catch (error) {
      if (valid()) {
        if (link && rejected(error)) invitation = undefined;
        signedOut(rejected(error) ? (link ? "The invitation is expired or already used." : undefined) : errorMessage(error),
          !rejected(error));
      }
    }
  }

  async function signOut(): Promise<void> {
    try {
      await exclusive(async () => { await auth.signOut(listener); renewId(""); });
      if (!life.signal.aborted) signedOut();
    } catch (error) { feedback(errorMessage(error), false, true); }
  }

  async function verifyOnReturn(): Promise<void> {
    const current = binding;
    if (!current || document.visibilityState === "hidden") return;
    try {
      const next = await grant(current.grant);
      if (binding === current) current.grant = next;
    } catch (error) {
      if (binding !== current) return;
      if (rejected(error) || (error instanceof ClientError && error.kind === "source")) {
        signedOut(rejected(error) ? "Device access expired or was revoked. Open a new invitation." : errorMessage(error));
      }
    }
  }

  search.hidden = true;
  composer.hidden = true;
  sidebarToggle.disabled = true;
  sidebar.setAttribute("aria-label", "Shared Session");
  const heading = sidebar.querySelector(".sidebar-title");
  if (heading) heading.textContent = "Session";
  sidebarToggle.setAttribute("aria-label", "Shared Session");
  sidebarToggle.title = "Shared Session";
  disconnectButton.setAttribute("aria-label", "Sign out");
  disconnectButton.title = "Sign out";
  refreshButton.setAttribute("aria-label", "Refresh shared Session");
  refreshButton.title = "Refresh shared Session";
  disconnectButton.hidden = true;
  refreshButton.hidden = true;
  narrow.addEventListener("change", () => drawer(!narrow.matches), listener);
  sidebarToggle.addEventListener("click", () => drawer(sidebarToggle.getAttribute("aria-expanded") !== "true"), listener);
  disconnectButton.addEventListener("click", () => void signOut(), listener);
  refreshButton.addEventListener("click", () => {
    void loadDirectory();
    void conversation?.check().catch(() => { /* The conversation already presents its read error. */ });
    approvals.refresh();
  }, listener);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "hidden") {
      const check = verifyOnReturn();
      foregroundCheck = check;
      void check.finally(() => { if (foregroundCheck === check) foregroundCheck = undefined; });
    }
    syncApprovals();
  }, listener);
  drawer(!narrow.matches);
  if (invitation) signedOut();
  else void connect();
  return () => { life.abort(); attempt?.abort(); retire(); };
}
