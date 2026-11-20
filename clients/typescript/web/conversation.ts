import { flushSync, mount, unmount } from "svelte";
import { ClientError, RemoteError, cursorId } from "../src/index.ts";
import type { Dtos, DurableCursor, FlowClient } from "../src/index.ts";
import ConversationPane from "./ConversationPane.svelte";
import type { ApprovalView } from "./approval-activity.ts";
import type { PendingApproval } from "../src/approvals.ts";
import { fitsImageSubmission, imageSizeMessage } from "./composer-images.ts";
import { createDraftObserver } from "./draft-observer.ts";
import type { DraftObservationState } from "./draft-observer.ts";
import { observeDurable } from "./durable-observer.ts";
import type { DurableObservationState } from "./durable-observer.ts";
import { observeInbox } from "./inbox.ts";
import type { InboxAction, InboxView, PendingInbox } from "./inbox.ts";
import { project } from "./live-projection.ts";
import type { State as ProjectionState } from "./live-projection.ts";
import { sameHistoryHead } from "./history-controller.ts";
import type { HistoryBinding, HistoryController } from "./history-controller.ts";
import type { DraftAttachments, PendingDraft, PersistDraftResult } from "./tab-drafts.ts";

type Page = Dtos["ConversationPage"];
type Item = Dtos["ConversationItem"];
export type Draft = {
  text: string;
  edit: number;
  attachments?: DraftAttachments;
  attachmentsMemoryOnly?: boolean;
  pending?: PendingDraft;
  busy?: boolean;
  feedback?: string;
  retryReady?: boolean;
  changes: EventTarget;
  persist?: () => PersistDraftResult;
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
  canShowHistory?: boolean;
  inbox?: InboxView;
  attachments?: DraftAttachments;
  attachmentsMemoryOnly?: boolean;
  settings?: Dtos["FlowComposerResponse"];
}>;

export type ConversationConsole = Readonly<{
  edit: (text: string) => void;
  inbox: (action: InboxAction) => void;
  send: () => void;
  checkDelivery: () => void;
  retry: () => void;
  jumpLatest: () => void;
  browseModels: (query: string, signal: AbortSignal) => Promise<Dtos["FlowComposerResponse"] | undefined>;
  showHistory?: (trigger: HTMLButtonElement) => void;
  addImages: (images: readonly Dtos["InlineImage"][]) => string | undefined;
  removeAttachment: (attachment: Dtos["InlineImage"] | "quote") => void;
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
  live?: boolean;
  history?: HistoryController;
  approvals?: {
    changes: EventTarget;
    current: () => ApprovalView | undefined;
    review: (view: ApprovalView, request: PendingApproval, trigger: HTMLButtonElement) => void;
  };
  inbox?: { pending: Map<string, PendingInbox>; edits: boolean };
  active: () => boolean;
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
  let projection: ProjectionState | undefined;
  let shown: Dtos["FlowDraftSnapshot"] | null = null;
  let pending: Dtos["FlowDraftSnapshot"] | undefined;
  const checkpoints = new Map<string, DurableCursor>();
  let frame = 0;
  let readingAnchor: { node: HTMLElement; top: number; manual: boolean } | undefined;
  const live = options.live ? createDraftObserver({ flow, signal: closed, onState: newDraftState }) : undefined;

  const inbox = options.inbox && observeInbox(flow, closed, () => verified && options.active(),
    options.inbox.pending, options.inbox.edits, syncComposer);

  const history: HistoryBinding = {
    flow, signal: closed, active: options.active, page: () => verified ? page : undefined,
    refresh: schedule, recoverable: () => !draft.pending && !draft.busy && !draft.text && !draft.attachments,
    recover: async (record, input, stillCurrent) => {
      const owner = draft;
      const edit = owner.edit;
      if (!history.recoverable() || record.status.state !== "applied") return false;
      const { result } = await flow.historyRecoverInput(record, input, { signal: closed });
      if (!stillCurrent() || closed.aborted || !options.active() || owner !== draft || owner.edit !== edit ||
        !sameHistoryHead(page?.head, result.head) || !history.recoverable()) return false;
      const { text, quote, images } = result;
      owner.text = text;
      if (quote != null || images.length > 0) owner.attachments = {
        ...(quote == null ? {} : { quote }),
        ...(images.length === 0 ? {} : { images }),
      };
      owner.edit++;
      if (owner.persist?.() !== "saved") {
        owner.attachmentsMemoryOnly = owner.attachments !== undefined;
        if (!owner.attachmentsMemoryOnly) owner.feedback = "Recovered draft is available in this tab until refresh.";
      }
      owner.changes.dispatchEvent(new Event("change"));
      return true;
    },
  };

  content.replaceChildren();
  const transcript = mount(ConversationPane, {
    target: content,
    props: {
      loadEarlier: () => { void read(true); },
      reviewApproval: (view, request, trigger) => {
        if (verified && options.active() && page) options.approvals?.review(view, request, trigger);
      },
      refresh: () => { void read(); },
      anchor: (node: HTMLElement) => {
        followingTail = false;
        readingAnchor = { node, top: node.getBoundingClientRect().top, manual: true };
      },
    },
  });
  function syncActions(): ConversationConsole {
    const owner = draft;
    const isCurrent = () => !closed.aborted && verified && owner === draft;
    return {
      inbox: (action) => { if (isCurrent()) inbox?.act(action); },
      browseModels: async (query, signal) => {
        if (!isCurrent() || !options.active() || signal.aborted || !settings?.origins) return;
        const { result } = await flow.composer({ catalog_query: query }, { signal: AbortSignal.any([signal, closed]) });
        if (isCurrent() && options.active() && !signal.aborted) return result;
      },
      showHistory: (trigger) => {
        if (isCurrent()) options.history?.branches(history, trigger);
      },
      addImages: (images) => {
        if (!isCurrent() || !options.active() || owner.pending || owner.busy || !images.length) return;
        const attachments = { ...owner.attachments, images: [...(owner.attachments?.images ?? []), ...images] };
        if (!fitsImageSubmission(owner.text, attachments)) return imageSizeMessage;
        owner.attachments = attachments;
        owner.edit++;
        saveDraft();
        syncComposer();
      },
      removeAttachment: (attachment) => {
        if (!isCurrent() || !options.active() || owner.pending || owner.busy || !owner.attachments) return;
        const { quote, images } = owner.attachments;
        if (attachment === "quote" ? quote === undefined : !images?.includes(attachment)) return;
        const nextQuote = attachment === "quote" ? undefined : quote;
        const nextImages = images?.filter((image) => image !== attachment);
        if (nextQuote === undefined && !nextImages?.length) delete owner.attachments;
        else owner.attachments = { ...(nextQuote === undefined ? {} : { quote: nextQuote }),
          ...(nextImages?.length ? { images: nextImages } : {}) };
        owner.edit++;
        saveDraft();
        syncComposer();
      },
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
        readingAnchor = undefined;
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
    const queue = inbox?.snapshot();
    options.console({
      text: draft.text,
      feedback: draft.feedback ?? "",
      editable: verified,
      canSend: verified && !draft.busy && draft.pending === undefined && (draft.text.trim() !== "" || draft.attachments !== undefined),
      canCheck: verified && draft.pending !== undefined && !draft.busy,
      canRetry: verified && !draft.busy && draft.retryReady === true && draft.pending?.text !== undefined,
      hasPending: draft.pending !== undefined,
      hasEarlierView: page !== undefined && !followingTail,
      canShowHistory: verified && options.history !== undefined,
      ...(queue === undefined ? {} : { inbox: queue }),
      ...(draft.attachments === undefined ? {} : { attachments: draft.attachments }),
      ...(draft.attachments && draft.attachmentsMemoryOnly ? { attachmentsMemoryOnly: true } : {}),
      ...(settings === undefined ? {} : { settings }),
    }, consoleActions);
  }

  function saveDraft(): void {
    if (saveTimer !== undefined) clearTimeout(saveTimer);
    saveTimer = undefined;
    const saved = draft.persist?.();
    const memoryOnly = draft.attachments !== undefined && saved !== "saved";
    if (Boolean(draft.attachmentsMemoryOnly) !== memoryOnly) {
      draft.attachmentsMemoryOnly = memoryOnly;
      syncComposer();
    }
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

  const syncApprovals = () => transcript.updateApprovals(verified && options.active() ? options.approvals?.current() : undefined);
  options.approvals?.changes.addEventListener("change", syncApprovals, { signal: closed });

  function syncTranscript(): void {
    syncApprovals();
    if (pending && page && pending.head.branch === page.head.branch && pending.head.version === page.head.version &&
        pending.after.sequence <= page.source.cursor.sequence &&
        (cursorId(page.source.cursor) === cursorId(pending.after) || checkpoints.has(cursorId(pending.after)))) {
      shown = pending;
      pending = undefined;
    }
    const previous = projection;
    const next = page === undefined ? undefined : project({
      flow: page.flow, head: page.head, cursor: page.source.cursor, items,
      snapshot: shown, verifiedCheckpoints: [...checkpoints.values(), ...(shown ? [shown.after] : [])],
    }, previous);
    projection = next?.state;
    transcript.update({
      rows: next?.rows ?? [],
      placeholder,
      hasEarlier: before != null,
      loading,
      ...(verified && options.history && page ? { history: {
        canRevert: (item) => page !== undefined && flow.canRevert(item, page.head),
        review: (item, trigger) => options.history?.revert(history, item, trigger),
      } } : {}),
    });
  }

  function rememberCheckpoint(receipt: DurableCursor): void {
    checkpoints.set(cursorId(receipt), receipt);
    if (checkpoints.size > 96) {
      const oldest = checkpoints.keys().next().value;
      if (oldest !== undefined) checkpoints.delete(oldest);
    }
  }

  function queueDraftDraw(): void {
    if (frame || closed.aborted) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      if (!closed.aborted && page) draw(followingTail, true);
    });
  }

  function newDraftState(state: DraftObservationState): void {
    if (state.state === "ready") {
      if (state.reset) shown = null;
      pending = state.snapshot;
      queueDraftDraw();
      if (page && pending.after.sequence > page.source.cursor.sequence) schedule();
    } else if (state.state === "head_reset" || state.state === "detached" || state.state === "stopped") {
      shown = null;
      pending = undefined;
      queueDraftDraw();
    } else if (state.state === "stale_head") schedule();
  }

  function updateReadingPosition(): void {
    const next = atTail();
    if (next !== followingTail) {
      followingTail = next;
      syncComposer();
    }
  }

  function draw(scrollToTail: boolean, preserveAnchor: boolean): void {
    const manual = preserveAnchor && readingAnchor?.manual && readingAnchor.node.isConnected;
    const keepTail = scrollToTail && !manual;
    const anchor = !keepTail && preserveAnchor ? visibleAnchor(content) : undefined;
    if (keepTail || !preserveAnchor) readingAnchor = undefined;
    else if (anchor && !readingAnchor?.node.isConnected)
      readingAnchor = { node: anchor, top: anchor.getBoundingClientRect().top, manual: false };
    flushSync(syncTranscript);
    if (keepTail) {
      content.scrollTop = content.scrollHeight;
    } else if (readingAnchor?.node.isConnected) {
      content.scrollTop += readingAnchor.node.getBoundingClientRect().top - readingAnchor.top;
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
        onBatch: (receipt) => {
          if (live) rememberCheckpoint(receipt);
          inbox?.invalidate(receipt);
          if (inbox) syncComposer();
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
          { ...(cursor == null ? {} : { before: cursor }), ...(live ? { include_source_parts: true } : {}) },
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
      const tail = followingTail;
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
        if (!sameSource) checkpoints.clear();
        page = next;
        if (live) {
          rememberCheckpoint(next.source.cursor);
          live.bind(next);
        }
        draw(tail, sameSource);
        if (!verified) {
          verified = true;
          options.verified?.();
        }
        options.executionChanged?.();
        useDraft(options.draftFor(next));
        inbox?.install(next.head, next.source.cursor);
        options.history?.installed(history);
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
    if (owner.edit === original.edit) {
      owner.text = "";
      delete owner.attachments;
    }
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
    if (retry ? !owner.retryReady || owner.pending?.text === undefined : owner.pending || (!owner.text.trim() && !owner.attachments)) return;
    const original = owner.pending ?? {
      request_id: crypto.randomUUID(),
      text: owner.text,
      head: page.head,
      edit: owner.edit,
      ...(owner.attachments === undefined ? {} : { attachments: owner.attachments }),
    };
    const text = original.text;
    if (text === undefined) return;
    if (original.attachments?.images?.length && !fitsImageSubmission(text, original.attachments)) {
      owner.feedback = imageSizeMessage;
      syncComposer();
      return;
    }
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
        user: { text, ...original.attachments },
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
    if (readingAnchor?.manual && readingAnchor.node.isConnected)
      content.scrollTop += readingAnchor.node.getBoundingClientRect().top - readingAnchor.top;
    else if (followingTail && page !== undefined) content.scrollTop = content.scrollHeight;
    else if (readingAnchor?.node.isConnected)
      content.scrollTop += readingAnchor.node.getBoundingClientRect().top - readingAnchor.top;
    updateReadingPosition();
  });
  resize.observe(content);
  const transcriptRoot = content.querySelector(".transcript");
  if (transcriptRoot) resize.observe(transcriptRoot);
  content.addEventListener("scroll", () => {
    const keep = readingAnchor?.node.isConnected &&
      (readingAnchor.manual || Math.abs(readingAnchor.node.getBoundingClientRect().top - readingAnchor.top) < 1);
    if (!keep) readingAnchor = undefined;
    updateReadingPosition();
  }, { signal: closed, passive: true });
  const releaseAnchor = () => { readingAnchor = undefined; };
  for (const type of ["pointerdown", "wheel", "touchmove"])
    content.addEventListener(type, releaseAnchor, { signal: closed, passive: true });
  content.addEventListener("keydown", (event) => {
    if (["ArrowDown", "ArrowUp", "PageDown", "PageUp", "Home", "End"].includes(event.key)) releaseAnchor();
  }, { signal: closed });
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
      cancelAnimationFrame(frame);
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
