import { flushSync, mount, unmount } from "svelte";
import { ClientError, RemoteError } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient } from "../src/index.ts";
import ConsoleComposer from "./ConsoleComposer.svelte";
import ConversationPane from "./ConversationPane.svelte";

type Page = Dtos["ConversationPage"];
type Item = Dtos["ConversationItem"];
type Pending = {
  request_id: string;
  text: string;
  head: Dtos["HistoryHead"];
  edit: number;
};
export type Draft = {
  text: string;
  edit: number;
  pending?: Pending;
  busy?: boolean;
  feedback?: string;
  changes: EventTarget;
};
type Status = (text: string, loading?: boolean, failed?: boolean) => void;

export type ConversationMemory = {
  page: Page;
  before: Dtos["ConversationBefore"] | null | undefined;
  items: readonly Item[];
  scrollTop: number;
  followingTail: boolean;
};

type ConversationOptions = {
  cached?: ConversationMemory;
  verified?: () => void;
  retain?: (memory: ConversationMemory) => void;
};

function cacheable(items: readonly Item[]): boolean {
  if (items.length > 250) return false;
  let textUnits = 0;
  for (const item of items) {
    if (item.body.kind !== "message") continue;
    for (const part of item.body.parts) {
      if (part.kind === "text" || part.kind === "quote" || part.kind === "thinking") {
        textUnits += part.text.length;
      }
    }
    if (textUnits > 256 * 1024) return false;
  }
  return true;
}

function recordKey({
  commit,
  fact,
  item,
  message,
}: Dtos["ConversationItemId"]): string {
  return [commit, fact, item, message].join(":");
}

function pageIdentity(page: Page): string {
  return [page.source.cursor.journal_id, page.head.branch, page.head.version].join(":");
}

function visibleAnchor(content: HTMLElement): HTMLElement | undefined {
  const items = content.querySelectorAll<HTMLElement>("[data-conversation-item]");
  const top = content.getBoundingClientRect().top;
  let begin = 0;
  let end = items.length;
  while (begin < end) {
    const middle = Math.floor((begin + end) / 2);
    if (items[middle]!.getBoundingClientRect().bottom > top) end = middle;
    else begin = middle + 1;
  }
  return items[begin];
}

export function showConversation(
  flow: FlowClient,
  content: HTMLElement,
  composer: HTMLElement,
  draft: Draft,
  status: Status,
  closed: AbortSignal,
  options: ConversationOptions = {},
): void {
  if (closed.aborted) return;
  const cached = options.cached;
  let page = cached?.page;
  let before = cached?.before;
  let items: readonly Item[] = cached?.items ?? [];
  let placeholder = "Loading conversation";
  let verified = false;
  let loading = false;
  let dirty = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let observation: AbortController | undefined;
  let streamEnded = false;
  let settings: Dtos["FlowComposerResponse"] | undefined;
  let followingTail = cached?.followingTail ?? true;

  content.replaceChildren();
  composer.replaceChildren();
  const transcript = mount(ConversationPane, {
    target: content,
    props: {
      loadEarlier: () => { void read(true); },
      refresh: () => { void read(); },
    },
  });
  const consoleView = mount(ConsoleComposer, {
    target: composer,
    props: {
      edit: (text: string) => {
        draft.text = text;
        draft.edit++;
        syncComposer();
      },
      send: () => { void submit(); },
      checkDelivery: () => { void checkDelivery(); },
      jumpLatest: () => {
        content.scrollTo({
          top: content.scrollHeight,
          behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "instant" : "smooth",
        });
      },
    },
  });

  function atTail(): boolean {
    const distance = content.scrollHeight - content.scrollTop - content.clientHeight;
    return distance <= 48;
  }

  function syncComposer(): void {
    if (closed.aborted) return;
    consoleView.update({
      text: draft.text,
      feedback: draft.feedback ?? "",
      canSend: verified && !draft.busy &&
        (draft.pending !== undefined || draft.text.trim() !== ""),
      canCheck: verified && draft.pending !== undefined && !draft.busy,
      hasPending: draft.pending !== undefined,
      hasEarlierView: page !== undefined && !followingTail,
      ...(settings === undefined ? {} : { settings }),
    });
  }

  function syncTranscript(): void {
    transcript.update({
      source: page === undefined ? "" : pageIdentity(page),
      items,
      placeholder,
      hasEarlier: before != null,
      loading,
    });
  }

  function updateReadingPosition(): void {
    const next = atTail();
    if (next !== followingTail) {
      followingTail = next;
      syncComposer();
    }
  }

  function draw(scrollToTail: boolean, preserveAnchor: boolean): void {
    const anchor = !scrollToTail && preserveAnchor ? visibleAnchor(content) : undefined;
    const oldPosition = anchor?.getBoundingClientRect().top;
    flushSync(syncTranscript);
    if (scrollToTail) {
      content.scrollTop = content.scrollHeight;
    } else if (anchor?.isConnected && oldPosition !== undefined) {
      content.scrollTop += anchor.getBoundingClientRect().top - oldPosition;
    } else if (!preserveAnchor) {
      content.scrollTop = 0;
    }
    updateReadingPosition();
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
      for await (const _batch of observer) schedule();
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
    syncTranscript();
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
          placeholder = "Conversation exceeds the browser read limit.";
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
      const tail = atTail();
      const sameSource = page !== undefined && pageIdentity(page) === pageIdentity(next);
      placeholder = "No messages yet";
      if (older) {
        before = next.before;
        items = [...next.items, ...items];
        draw(false, true);
      } else {
        const lastLoaded = items.at(-1);
        const lastKey = lastLoaded === undefined ? undefined : recordKey(lastLoaded.id);
        const canMerge = sameSource && lastKey !== undefined &&
          next.items.some((item) => recordKey(item.id) === lastKey);
        const first = next.items[0];
        const seam = canMerge && first
          ? items.findIndex((item) => recordKey(item.id) === recordKey(first.id))
          : -1;
        if (seam > 0) items = [...items.slice(0, seam), ...next.items];
        else {
          before = next.before;
          items = next.items;
        }
        page = next;
        draw(tail, sameSource);
        if (!verified) {
          verified = true;
          options.verified?.();
        }
        if (observation === undefined || streamEnded)
          void observe(next.source.cursor);
      }
      status("");
    } catch (error) {
      if (!closed.aborted) {
        const reason =
          error instanceof Error ? error.message : "Conversation read failed";
        if (page === undefined) placeholder = reason;
        status(reason, false, true);
      }
    } finally {
      loading = false;
      if (!closed.aborted) {
        syncTranscript();
        syncComposer();
        if (dirty) schedule();
      }
    }
  }

  function accept(original: Pending): void {
    if (draft.pending !== original) return;
    delete draft.pending;
    if (draft.edit === original.edit) draft.text = "";
    delete draft.feedback;
    draft.changes.dispatchEvent(new Event("accepted"));
  }

  async function checkDelivery(): Promise<void> {
    const original = draft.pending;
    if (!verified || !original || draft.busy) return;
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
    if (closed.aborted || !verified || draft.busy || !page ||
      (!draft.pending && draft.text.trim() === "")) return;
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
      // The receipt belongs to the captured draft even after the view closes.
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

  const resize = new ResizeObserver(() => {
    if (followingTail && page !== undefined) content.scrollTop = content.scrollHeight;
    updateReadingPosition();
  });
  resize.observe(content);
  content.addEventListener("scroll", updateReadingPosition, { signal: closed, passive: true });
  draft.changes.addEventListener("change", syncComposer, { signal: closed });
  draft.changes.addEventListener("accepted", schedule, { signal: closed });
  closed.addEventListener(
    "abort",
    () => {
      if (verified && page && options.retain) {
        const retained = cacheable(items);
        if (retained || cacheable(page.items)) {
          options.retain({
            page,
            before: retained ? before : page.before,
            items: retained ? items : page.items,
            scrollTop: retained ? content.scrollTop : 0,
            followingTail: !retained || followingTail,
          });
        }
      }
      observation?.abort();
      if (timer !== undefined) clearTimeout(timer);
      resize.disconnect();
      void unmount(transcript);
      void unmount(consoleView);
    },
    { once: true },
  );
  void flow.composer({}, { signal: closed }).then(({ result }) => {
    if (closed.aborted) return;
    settings = result;
    syncComposer();
  }).catch(() => {
    // Unloaded targets and older hosts have no settings to display.
  });
  if (cached) {
    flushSync(syncTranscript);
    content.scrollTop = cached.followingTail ? content.scrollHeight : cached.scrollTop;
  }
  syncComposer();
  void read();
}
