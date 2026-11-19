import { flushSync, mount, unmount } from "svelte";
import { ClientError, RemoteError } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient } from "../src/index.ts";
import ConversationPane from "./ConversationPane.svelte";
import { observeDurable } from "./durable-observer.ts";
import type { DurableObservationState } from "./durable-observer.ts";
import type { PendingDraft } from "./tab-drafts.ts";

type Page = Dtos["ConversationPage"];
type Item = Dtos["ConversationItem"];
export type Draft = {
  text: string;
  edit: number;
  pending?: PendingDraft;
  busy?: boolean;
  feedback?: string;
  retryReady?: boolean;
  changes: EventTarget;
  persist?: () => void;
};
export type ConsoleSnapshot = Readonly<{
  text: string;
  feedback: string;
  canSend: boolean;
  canCheck: boolean;
  canRetry: boolean;
  editable: boolean;
  hasPending: boolean;
  hasEarlierView: boolean;
  settings?: Dtos["FlowComposerResponse"];
}>;

export type ConversationConsole = Readonly<{
  edit: (text: string) => void;
  send: () => void;
  checkDelivery: () => void;
  retry: () => void;
  jumpLatest: () => void;
}>;

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
  executionChanged?: () => void;
  retain?: (memory: ConversationMemory) => void;
  draftFor: (page: Page) => Draft;
  console: (snapshot: ConsoleSnapshot, actions: ConversationConsole) => void;
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
  status: Status,
  closed: AbortSignal,
  options: ConversationOptions,
): void {
  if (closed.aborted) return;
  const cached = options.cached;
  let page = cached?.page;
  let before = cached?.before;
  let items: readonly Item[] = cached?.items ?? [];
  let placeholder = "Loading conversation";
  let verified = false;
  let draft: Draft = { text: "", edit: 0, changes: new EventTarget() };
  let saveTimer: ReturnType<typeof setTimeout> | undefined;
  let loading = false;
  let dirty = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let reading: Promise<Page | undefined> | undefined;
  let observing = false;
  let observationState: DurableObservationState | undefined;
  let readError = "";
  let settings: Dtos["FlowComposerResponse"] | undefined;
  let followingTail = cached?.followingTail ?? true;

  content.replaceChildren();
  const transcript = mount(ConversationPane, {
    target: content,
    props: {
      loadEarlier: () => { void read(true); },
      refresh: () => { void read(); },
    },
  });
  function syncActions(): ConversationConsole {
    const owner = draft;
    const isCurrent = () => !closed.aborted && verified && owner === draft;
    return {
      edit: (text) => {
        if (!isCurrent()) return;
        owner.text = text;
        owner.edit++;
        if (saveTimer !== undefined) clearTimeout(saveTimer);
        saveTimer = setTimeout(saveDraft, 150);
        syncComposer();
      },
      send: () => { if (isCurrent()) void submit(); },
      checkDelivery: () => { if (isCurrent()) void checkDelivery(); },
      retry: () => { if (isCurrent()) void submit(true); },
      jumpLatest: () => {
        if (!isCurrent()) return;
        content.scrollTo({
          top: content.scrollHeight,
          behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "instant" : "smooth",
        });
      },
    };
  }

  let consoleOwner: Draft | undefined;
  let consoleActions: ConversationConsole;

  function atTail(): boolean {
    const distance = content.scrollHeight - content.scrollTop - content.clientHeight;
    return distance <= 48;
  }

  function syncComposer(): void {
    if (closed.aborted) return;
    if (consoleOwner !== draft) {
      consoleOwner = draft;
      consoleActions = syncActions();
    }
    options.console({
      text: draft.text,
      feedback: draft.feedback ?? "",
      editable: verified,
      canSend: verified && !draft.busy && draft.pending === undefined && draft.text.trim() !== "",
      canCheck: verified && draft.pending !== undefined && !draft.busy,
      canRetry: verified && !draft.busy && draft.retryReady === true && draft.pending?.text !== undefined,
      hasPending: draft.pending !== undefined,
      hasEarlierView: page !== undefined && !followingTail,
      ...(settings === undefined ? {} : { settings }),
    }, consoleActions);
  }

  function saveDraft(): void {
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    saveTimer = undefined;
    draft.persist?.();
  }

  function useDraft(next: Draft): void {
    if (draft === next) return;
    saveDraft();
    draft.changes.removeEventListener("change", syncComposer);
    draft.changes.removeEventListener("accepted", schedule);
    draft = next;
    draft.changes.addEventListener("change", syncComposer, { signal: closed });
    draft.changes.addEventListener("accepted", schedule, { signal: closed });
    if (draft.pending && !draft.busy) void checkDelivery();
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

  function syncStatus(): void {
    if (closed.aborted) return;
    if (readError) {
      status(readError, false, true);
    } else if (observationState?.state === "checking_source") {
      status("Checking conversation source", true);
    } else if (observationState?.state === "reconnecting") {
      status("Reconnecting", true);
    } else if (observationState?.state === "stopped") {
      const reason = observationState.reason;
      const failures = {
        authentication: "Connection expired. Reconnect to continue.",
        authorization: "Live updates are unavailable for this Flow.",
        contract: "This host cannot provide live updates.",
        source: "Conversation source changed. Refresh to check again.",
        unavailable: "Live updates are unavailable. Refresh to retry.",
      };
      status(failures[reason], false, true);
    } else {
      status("");
    }
  }

  async function observe(after: Readonly<DurableCursor>): Promise<void> {
    observing = true;
    try {
      await observeDurable({
        source: flow.address,
        after,
        signal: closed,
        open: (cursor, signal) => flow.observeEvents({ after: cursor }, { signal }),
        refresh: async (signal) => {
          await reading?.catch(() => undefined);
          if (signal.aborted) return undefined;
          return read(false, true);
        },
        onBatch: () => {
          // Conversation and execution retain their own typed read boundaries.
          options.executionChanged?.();
          schedule();
        },
        onState: (state) => {
          observationState = state;
          syncStatus();
        },
      });
    } finally {
      observing = false;
    }
  }

  function read(older = false, propagate = false): Promise<Page | undefined> {
    if (reading) return reading;
    reading = readPage(older, propagate).finally(() => { reading = undefined; });
    return reading;
  }

  async function readPage(older: boolean, propagate: boolean): Promise<Page | undefined> {
    if (closed.aborted) return;
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
        readError = result.kind === "readers_busy"
          ? "Conversation readers are busy. Refresh to retry."
          : "Conversation read limit reached.";
        syncStatus();
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
        options.executionChanged?.();
        useDraft(options.draftFor(next));
        if (!observing)
          void observe(next.source.cursor);
      }
      readError = "";
      syncStatus();
      return next;
    } catch (error) {
      if (!closed.aborted) {
        readError =
          error instanceof Error ? error.message : "Conversation read failed";
        if (page === undefined) placeholder = readError;
        syncStatus();
      }
      if (propagate) throw error;
    } finally {
      loading = false;
      if (!closed.aborted) {
        syncTranscript();
        syncComposer();
        if (dirty) schedule();
      }
    }
  }

  function accept(owner: Draft, original: PendingDraft): void {
    if (owner.pending !== original) return;
    delete owner.pending;
    delete owner.retryReady;
    if (owner.edit === original.edit) owner.text = "";
    delete owner.feedback;
    owner.persist?.();
    owner.changes.dispatchEvent(new Event("accepted"));
  }

  async function checkDelivery(): Promise<void> {
    const owner = draft;
    const original = owner.pending;
    if (!verified || !original || owner.busy) return;
    owner.busy = true;
    owner.retryReady = false;
    owner.feedback = "Checking delivery";
    syncComposer();
    try {
      const known = await flow.inputStatus(original.request_id, {
        signal: closed,
      });
      if (known.result.observation) accept(owner, original);
      else {
        owner.retryReady = original.text !== undefined;
        owner.feedback = owner.retryReady
          ? "Delivery unknown"
          : "Delivery unknown. The original text is unavailable for retry.";
      }
    } catch (error) {
      if (!closed.aborted)
        owner.feedback =
          error instanceof Error ? error.message : "Delivery check failed.";
    } finally {
      delete owner.busy;
      owner.changes.dispatchEvent(new Event("change"));
    }
  }

  async function submit(retry = false): Promise<void> {
    const owner = draft;
    if (closed.aborted || !verified || owner.busy || !page) return;
    if (retry ? !owner.retryReady || owner.pending?.text === undefined : owner.pending || !owner.text.trim()) return;
    const original = owner.pending ?? {
      request_id: crypto.randomUUID(),
      text: owner.text,
      head: page.head,
      edit: owner.edit,
    };
    const text = original.text;
    if (text === undefined) return;
    owner.pending = original;
    owner.busy = true;
    owner.retryReady = false;
    owner.feedback = "Sending";
    owner.persist?.();
    syncComposer();
    try {
      // The receipt belongs to the captured draft even after the view closes.
      await flow.submit({
        request_id: original.request_id,
        expected_head: original.head,
        user: { text },
      });
      accept(owner, original);
    } catch (error) {
      const rejected =
        (error instanceof ClientError && error.outcome === "not_sent") ||
        (error instanceof RemoteError && error.code === -32002);
      if (rejected && owner.pending === original) {
        delete owner.pending;
        owner.persist?.();
        if (owner === draft) schedule();
      }
      owner.feedback = rejected
        ? error instanceof Error
          ? error.message
          : "Send rejected"
        : error instanceof RemoteError ? error.message + " · Check delivery" : "Delivery unknown";
    } finally {
      delete owner.busy;
      owner.changes.dispatchEvent(new Event("change"));
    }
  }

  const resize = new ResizeObserver(() => {
    if (content.clientHeight === 0) return;
    if (followingTail && page !== undefined) content.scrollTop = content.scrollHeight;
    updateReadingPosition();
  });
  resize.observe(content);
  content.addEventListener("scroll", updateReadingPosition, { signal: closed, passive: true });
  window.addEventListener("pagehide", saveDraft, { signal: closed });
  closed.addEventListener(
    "abort",
    () => {
      saveDraft();
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
      if (timer !== undefined) clearTimeout(timer);
      resize.disconnect();
      void unmount(transcript);
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
