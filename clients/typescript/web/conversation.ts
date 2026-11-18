import { ClientError, RemoteError } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient } from "../src/index.ts";
import { composerSettings } from "./composer-settings.ts";

type Page = Dtos["ConversationPage"];
type Item = Dtos["ConversationItem"];
type Pending = {
  request_id: string;
  text: string;
  head: Dtos["HistoryHead"];
  edit: number;
};
type Draft = {
  text: string;
  edit: number;
  pending?: Pending;
  busy?: boolean;
  feedback?: string;
  changes: EventTarget;
};
type Status = (text: string, loading?: boolean, failed?: boolean) => void;

function element<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  text = "",
  style?: string,
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  node.textContent = text;
  if (style) node.className = style;
  return node;
}

function command(label: string, text = label): HTMLButtonElement {
  const node = element("button", text);
  node.type = "button";
  node.setAttribute("aria-label", label);
  node.title = label;
  return node;
}

function recordKey({
  commit,
  fact,
  item,
  message,
}: Dtos["ConversationItemId"]): string {
  return [commit, fact, item, message].join(":");
}

function sameHead(a: Dtos["HistoryHead"], b: Dtos["HistoryHead"]): boolean {
  return a.branch === b.branch && a.version === b.version;
}

function renderItem(item: Item): HTMLElement {
  const body = item.body;
  const row = element("article", "", "message");
  if (body.kind === "deferred") {
    row.append(
      element("p", "Message exceeds the browser page limit.", "message-note"),
    );
    return row;
  }
  row.classList.add(`message-${body.role}`);
  for (const part of body.parts) {
    switch (part.kind) {
      case "text":
        row.append(element("p", part.text));
        break;
      case "quote":
        row.append(element("blockquote", part.text));
        break;
      case "thinking": {
        const detail = element("details");
        detail.append(element("summary", "Thinking"), element("p", part.text));
        row.append(detail);
        break;
      }
      case "tool_call":
        row.append(element("p", part.name, "message-note"));
        break;
      case "tool_result":
        row.append(
          element(
            "p",
            part.failed ? "Tool failed" : "Tool completed",
            "message-note",
          ),
        );
        break;
      case "image":
        row.append(element("p", "Image attachment", "message-note"));
        break;
    }
  }
  if (!row.hasChildNodes()) row.hidden = true;
  return row;
}

export function showConversation(
  flow: FlowClient,
  content: HTMLElement,
  composer: HTMLElement,
  draft: Draft,
  status: Status,
  closed: AbortSignal,
): void {
  const history = element("div", "", "conversation");
  const earlier = command("Load older messages", "↑ Earlier");
  const messages = element("div", "", "messages");
  const placeholder = element("p", "Loading conversation", "message-note");
  const refresh = command("Refresh conversation", "↻");
  const controls = element("div", "", "conversation-controls");
  earlier.hidden = true;
  controls.append(earlier, refresh);
  history.append(controls, placeholder, messages);
  content.replaceChildren(history);

  const form = element("form", "", "composer");
  const input = element("textarea");
  input.placeholder = "Message";
  input.setAttribute("aria-label", "Message");
  input.rows = 3;
  input.value = draft.text;
  const actions = element("div", "", "composer-actions");
  const check = command("Check original send", "Check delivery");
  const feedback = element("span", "", "message-note send-feedback");
  feedback.setAttribute("role", "status");
  const send = command("Send message", "↑");
  send.type = "submit";
  actions.append(feedback, check, send);
  form.append(input, composerSettings(flow, closed), actions);
  composer.replaceChildren(form);

  let page: Page | undefined;
  let before: Dtos["ConversationBefore"] | null | undefined;
  let items: readonly Item[] = [];
  const nodes = new Map<string, HTMLElement>();
  let loading = false;
  let dirty = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let observation: AbortController | undefined;
  let streamEnded = false;

  function syncComposer(): void {
    if (closed.aborted) return;
    if (input.value !== draft.text) input.value = draft.text;
    feedback.textContent = draft.feedback ?? "";
    send.disabled =
      page === undefined ||
      draft.busy === true ||
      (!draft.pending && draft.text.trim() === "");
    check.hidden = draft.pending === undefined;
    check.disabled = draft.busy === true;
    send.title = draft.pending ? "Retry original send" : "Send message";
    send.setAttribute("aria-label", send.title);
  }

  function draw(next: readonly Item[], scrollToTail: boolean): void {
    const keep = new Set(next.map((item) => recordKey(item.id)));
    for (const [key, node] of nodes) {
      if (!keep.has(key)) {
        node.remove();
        nodes.delete(key);
      }
    }
    let previous: ChildNode | null = null;
    for (const item of next) {
      const key = recordKey(item.id);
      let node = nodes.get(key);
      if (!node) {
        node = renderItem(item);
        nodes.set(key, node);
      }
      const following: ChildNode | null = previous
        ? previous.nextSibling
        : messages.firstChild;
      if (node !== following) messages.insertBefore(node, following);
      previous = node;
    }
    placeholder.hidden = next.length > 0;
    placeholder.textContent = "No messages yet";
    earlier.hidden = before == null;
    if (scrollToTail) content.scrollTop = content.scrollHeight;
  }

  function schedule(): void {
    dirty = true;
    if (loading || timer !== undefined || closed.aborted) return;
    timer = setTimeout(() => {
      timer = undefined;
      void read();
    }, 200);
  }

  async function observe(after: Readonly<DurableCursor>): Promise<void> {
    observation?.abort();
    const stream = new AbortController();
    observation = stream;
    streamEnded = false;
    try {
      const observer = await flow.observeEvents(
        { after },
        { signal: stream.signal },
      );
      for await (const _batch of observer) {
        schedule();
      }
    } catch {
      if (stream.signal.aborted) return;
    }
    if (!stream.signal.aborted) {
      streamEnded = true;
      status("Live updates disconnected. Refresh to reconnect.", false, true);
    }
  }

  async function read(older = false): Promise<void> {
    if (closed.aborted || loading) return;
    const cursor = older ? before : undefined;
    if (older && cursor == null) return;
    loading = true;
    dirty = false;
    earlier.disabled = true;
    refresh.disabled = true;
    try {
      const result = (
        await flow.conversation(
          { ...(cursor == null ? {} : { before: cursor }) },
          { signal: closed },
        )
      ).result;
      if (closed.aborted) return;
      if (result.state === "capacity") {
        if (page === undefined)
          placeholder.textContent =
            "Conversation exceeds the browser read limit.";
        status(
          result.kind === "readers_busy"
            ? "Conversation readers are busy. Refresh to retry."
            : "Conversation read limit reached.",
          false,
          true,
        );
        return;
      }
      const next = result.page;
      const tail =
        content.scrollHeight - content.scrollTop <= content.clientHeight + 48;
      if (older) {
        const height = content.scrollHeight;
        const offset = content.scrollTop;
        before = next.before;
        items = [...next.items, ...items];
        draw(items, false);
        content.scrollTop = offset + content.scrollHeight - height;
      } else {
        const lastLoadedIsStillVisible =
          page !== undefined &&
          sameHead(page.head, next.head) &&
          next.items.some(
            (item) =>
              recordKey(item.id) ===
              (items.length
                ? recordKey(items[items.length - 1]!.id)
                : undefined),
          );
        if (lastLoadedIsStillVisible && next.items.length) {
          const first = recordKey(next.items[0]!.id);
          const seam = items.findIndex((item) => recordKey(item.id) === first);
          if (seam > 0) items = [...items.slice(0, seam), ...next.items];
          else {
            before = next.before;
            items = next.items;
          }
        } else {
          before = next.before;
          items = next.items;
        }
        page = next;
        draw(items, tail);
        if (observation === undefined || streamEnded)
          void observe(next.source.cursor);
      }
      status("");
    } catch (error) {
      if (!closed.aborted) {
        const reason =
          error instanceof Error ? error.message : "Conversation read failed";
        if (page === undefined) placeholder.textContent = reason;
        status(reason, false, true);
      }
    } finally {
      loading = false;
      earlier.disabled = false;
      refresh.disabled = false;
      syncComposer();
      if (dirty) schedule();
    }
  }

  function accept(original: Pending): void {
    if (draft.pending !== original) return;
    delete draft.pending;
    if (draft.edit === original.edit) {
      draft.text = "";
      if (!closed.aborted) input.value = "";
    }
    delete draft.feedback;
    draft.changes.dispatchEvent(new Event("accepted"));
  }

  async function checkDelivery(): Promise<void> {
    const original = draft.pending;
    if (!original || draft.busy) return;
    draft.busy = true;
    syncComposer();
    try {
      const known = await flow.inputStatus(original.request_id, {
        signal: closed,
      });
      if (known.result.observation) accept(original);
      else draft.feedback = "Delivery unknown. Retry the original send.";
    } catch (error) {
      if (!closed.aborted)
        draft.feedback =
          error instanceof Error ? error.message : "Delivery check failed.";
    } finally {
      delete draft.busy;
      draft.changes.dispatchEvent(new Event("change"));
    }
  }

  async function submit(): Promise<void> {
    if (draft.busy || !page || (!draft.pending && draft.text.trim() === ""))
      return;
    const original = draft.pending ?? {
      request_id: crypto.randomUUID(),
      text: draft.text,
      head: page.head,
      edit: draft.edit,
    };
    draft.pending = original;
    draft.busy = true;
    draft.feedback = "Sending";
    syncComposer();
    try {
      await flow.submit({
        request_id: original.request_id,
        expected_head: original.head,
        user: { text: original.text },
      });
      accept(original);
    } catch (error) {
      const rejected =
        (error instanceof ClientError && error.outcome === "not_sent") ||
        (error instanceof RemoteError && error.code === -32002);
      if (rejected && draft.pending === original) {
        delete draft.pending;
        schedule();
      }
      draft.feedback = rejected
        ? error instanceof Error
          ? error.message
          : "Send rejected"
        : "Delivery unknown. Check or retry the original send.";
    } finally {
      delete draft.busy;
      draft.changes.dispatchEvent(new Event("change"));
    }
  }

  earlier.addEventListener("click", () => {
    void read(true);
  });
  refresh.addEventListener("click", () => {
    void read();
  });
  check.addEventListener("click", () => {
    void checkDelivery();
  });
  draft.changes.addEventListener("change", syncComposer, { signal: closed });
  draft.changes.addEventListener("accepted", schedule, { signal: closed });
  input.addEventListener("input", () => {
    draft.text = input.value;
    draft.edit++;
    syncComposer();
  });
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    void submit();
  });
  input.addEventListener("keydown", (event) => {
    if (
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.isComposing &&
      event.keyCode !== 229
    ) {
      event.preventDefault();
      if (!event.repeat && !send.disabled) form.requestSubmit();
    }
  });
  closed.addEventListener(
    "abort",
    () => {
      observation?.abort();
      if (timer !== undefined) clearTimeout(timer);
    },
    { once: true },
  );
  syncComposer();
  void read();
}

export type { Draft };
