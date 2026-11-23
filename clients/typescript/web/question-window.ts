import { flushSync, mount, unmount } from "svelte";
import type { Dtos, FlowClient } from "../src/index.ts";
import ConversationPane from "./ConversationPane.svelte";
import { fitsImageSubmission, imageSizeMessage } from "./composer-images.ts";
import type { ConsoleSnapshot, ConversationConsole } from "./conversation.ts";
import { retryableReadFailure, waitForReadRetry } from "./durable-observer.ts";
import { project, type State as Projection } from "./live-projection.ts";
import type { QuestionDisposal, QuestionDraft, QuestionDrafts, QuestionSend, QuestionSource, QuestionStop } from "./question-drafts.ts";
import type { DraftAttachments, DraftBinding, PersistDraftResult } from "./tab-drafts.ts";

export const questionMethods = ["question.status", "question.start", "question.transcript",
  "question.followup", "question.followup_status", "question.discard", "question.discard_status"];
export const questionStopMethods = ["question.execution", "question.stop"];

type Position = Dtos["QuestionPosition"];
type Execution = Dtos["QuestionExecutionObservation"];
type Accepted = { owner: string; receipt: Dtos["QuestionStartReceipt"] };
type Render = (snapshot: ConsoleSnapshot, actions: ConversationConsole) => void;
type Archive = { drafts: QuestionDrafts; binding: DraftBinding };

export class QuestionWindow {
  readonly #ids: QuestionDraft["ids"];
  readonly source: QuestionSource;
  readonly #changes = new EventTarget();
  readonly #items: Dtos["ConversationItem"][] = [];
  #owner: string | undefined;
  #accepted: Accepted | undefined;
  #tail: Position | null = null;
  #text = "";
  #edit = 0;
  #attachments: DraftAttachments | undefined;
  #pending: QuestionSend | undefined;
  #busy = false;
  #retry = false;
  #retired = false;
  #notice = "";
  #read: AbortController | undefined;
  #disposal: QuestionDisposal | undefined;
  #stop: QuestionStop | undefined;
  #stopWriting = false;
  #stopNotice = "";
  #saveTimer: ReturnType<typeof setTimeout> | undefined;
  #saved: PersistDraftResult | undefined;

  constructor(readonly flow: FlowClient, source: QuestionSource, readonly connection: AbortSignal,
    readonly archive?: Archive, readonly canStop = false) {
    const restored = archive?.drafts.get(archive.binding);
    this.source = restored?.anchor ?? source;
    this.#ids = restored?.ids ?? { question_id: crypto.randomUUID(), request_id: crypto.randomUUID() };
    this.#owner = restored?.owner;
    if (restored?.receipt && restored.owner) this.#accepted = { owner: restored.owner, receipt: restored.receipt };
    this.#edit = restored?.edit ?? 0;
    this.#text = restored?.text ?? (restored?.pending?.edit === this.#edit ? restored.pending?.user?.text ?? "" : "");
    const authored = restored?.pending?.edit === this.#edit ? restored.pending?.user : undefined;
    this.#attachments = restored?.attachments ?? (authored && (authored.images?.length || authored.quote != null) ? {
      ...(authored.images?.length ? { images: authored.images } : {}),
      ...(authored.quote == null ? {} : { quote: authored.quote }),
    } : undefined);
    this.#pending = restored?.pending;
    this.#disposal = restored?.disposal;
    this.#stop = restored?.stop;
  }

  get remains(): boolean { return !this.#retired && (this.#accepted !== undefined || this.#pending !== undefined); }

  flushDraft(): void {
    if (this.#saveTimer !== undefined) clearTimeout(this.#saveTimer);
    this.#saveTimer = undefined;
    if (!this.archive) return;
    const { drafts, binding } = this.archive;
    if (this.#retired || (!this.remains && !this.#text && !this.#attachments)) {
      drafts.remove(binding);
      return;
    }
    this.#saved = drafts.save(binding, {
      anchor: this.source, ids: this.#ids, edit: this.#edit, text: this.#text,
      ...(this.#owner ? { owner: this.#owner } : {}), ...(this.#accepted ? { receipt: this.#accepted.receipt } : {}),
      ...(this.#attachments ? { attachments: this.#attachments } : {}), ...(this.#pending ? { pending: this.#pending } : {}),
      ...(this.#disposal ? { disposal: this.#disposal } : {}),
      ...(this.#stop ? { stop: this.#stop } : {}),
    });
  }

  close(): void {
    if (this.remains || this.connection.aborted) this.flushDraft();
    else {
      if (this.#saveTimer !== undefined) clearTimeout(this.#saveTimer);
      this.#saveTimer = undefined;
      if (this.archive) this.archive.drafts.remove(this.archive.binding);
    }
  }

  #change(persist = false): void {
    if (persist) this.flushDraft();
    this.#changes.dispatchEvent(new Event("change"));
  }

  #scope(accepted: Accepted) {
    return { ...this.#ids, owner_generation: accepted.owner, host_generation: accepted.receipt.launch.host_generation };
  }

  #accept(pending: QuestionSend): void {
    if (this.#pending !== pending) return;
    if (pending.edit === this.#edit) {
      this.#text = "";
      this.#attachments = undefined;
      this.#edit++;
    }
    this.#pending = undefined;
    this.#retry = false;
    this.#notice = "";
  }

  async #submit(retry = false): Promise<void> {
    if (this.#busy || this.#retired || this.connection.aborted || this.#disposal) return;
    if (retry ? !this.#pending?.user || !this.#retry : !this.#canSend()) return;
    const pending = this.#pending ?? { id: this.#accepted ? crypto.randomUUID() : this.#ids.request_id,
      user: { text: this.#text, ...this.#attachments }, edit: this.#edit, ...(this.#tail ? { observed: this.#tail } : {}) };
    if (!pending.user) return;
    this.#pending = pending;
    this.#stopNotice = "";
    this.#retry = false;
    this.#busy = true;
    this.#notice = "Sending…";
    this.#change(true);
    try {
      const options = { signal: this.connection };
      if (pending.observed && this.#accepted) {
        await this.flow.questionFollowup({ ...this.#scope(this.#accepted), followup_request_id: pending.id,
          observed: pending.observed, user: pending.user, model: "smart" }, options);
      } else {
        if (this.#owner === undefined) {
          const { result } = await this.flow.questionStatus(this.#ids, options);
          if (result.owner_generation == null) throw new Error("Open the source Flow to ask a question.");
          this.#owner = result.owner_generation;
          this.flushDraft();
        }
        const { result } = await this.flow.startQuestion({ ...this.#ids, owner_generation: this.#owner,
          observed: this.source.source, expected_head: this.source.head, user: pending.user, model: "smart" }, options);
        this.#accepted = { owner: result.owner_generation, receipt: result.receipt };
      }
      this.#accept(pending);
    } catch (error) {
      this.#notice = error instanceof Error ? error.message : "Could not confirm delivery.";
    } finally {
      this.#busy = false;
      this.#change(true);
    }
  }

  async #check(): Promise<void> {
    const pending = this.#pending;
    if (!pending || this.#busy || this.connection.aborted || this.#retired) return;
    this.#busy = true;
    this.#notice = "Checking delivery…";
    this.#change();
    try {
      if (pending.observed && this.#accepted) {
        const { result } = await this.flow.questionFollowupStatus({ ...this.#scope(this.#accepted), followup_request_id: pending.id },
          { signal: this.connection });
        if (result.followup.state === "observed") this.#accept(pending);
        else if (result.followup.state === "retired") this.#retire();
        else this.#unconfirmed(result.followup.state === "unseen");
      } else {
        const { result } = await this.flow.questionStatus({ ...this.#ids, owner_generation: this.#owner ?? null }, { signal: this.connection });
        if (result.attempt.state === "accepted" && result.owner_generation) {
          this.#owner = result.owner_generation;
          this.#accepted = { owner: result.owner_generation, receipt: result.attempt.receipt };
          this.#accept(pending);
        } else if (result.attempt.state === "retired") this.#retire();
        else if (result.attempt.state === "preparing") this.#notice = "Preparing the question…";
        else this.#unconfirmed(result.owner_generation != null);
      }
    } catch (error) {
      this.#notice = error instanceof Error ? error.message : "Could not check delivery.";
    } finally {
      this.#busy = false;
      this.#change(true);
    }
  }

  #retire(): void {
    this.#retired = true;
    this.#notice = "This question has ended.";
    this.#pending = undefined;
    this.#change(true);
  }

  #unconfirmed(retry: boolean): void {
    this.#retry = retry && this.#pending?.user !== undefined;
    this.#notice = this.#pending?.user ? "Delivery is unconfirmed." : "Only the request identity is available for delivery lookup.";
  }

  #canSend(): boolean {
    return !this.#retired && !this.connection.aborted && !this.#disposal && !this.#busy && !this.#pending
      && (!this.#accepted || this.#tail !== null) && Boolean(this.#text.trim() || this.#attachments);
  }

  async #sendStop(operation: QuestionStop): Promise<void> {
    const accepted = this.#accepted;
    if (!accepted || this.#stopWriting || this.#retired || this.#disposal || this.connection.aborted) return;
    this.#stop = operation;
    this.#stopWriting = true;
    this.#stopNotice = "Stopping…";
    this.#change(true);
    try {
      await this.flow.questionStop({ ...this.#scope(accepted), ...operation }, { signal: this.connection });
    } catch {
      this.#stopNotice = "Checking stop…";
    } finally {
      this.#stopWriting = false;
      this.#change();
    }
  }

  async #discard(): Promise<void> {
    const observed = this.#tail ?? this.#disposal?.observed;
    if (!this.#accepted || !observed || this.#busy || this.#retired || this.connection.aborted) return;
    const operation = this.#disposal ??= { id: crypto.randomUUID(), observed };
    const scope = { ...this.#scope(this.#accepted), discard_request_id: operation.id };
    const options = { signal: this.connection };
    this.#busy = true;
    this.#notice = "Discarding…";
    this.#change(true);
    try {
      const { result } = await this.flow.questionDiscardStatus(scope, options);
      if (result.discard !== "discarded" && result.discard !== "retired")
        await this.flow.questionDiscard({ ...scope, observed: operation.observed }, options);
      this.#retire();
    } catch (error) {
      this.#notice = error instanceof Error ? error.message : "Could not confirm discard.";
    } finally {
      this.#busy = false;
      this.#change(true);
    }
  }

  async #checkDiscard(signal: AbortSignal): Promise<void> {
    const disposal = this.#disposal;
    const accepted = this.#accepted;
    if (!disposal || !accepted || this.#busy) return;
    try {
      const { result } = await this.flow.questionDiscardStatus({ ...this.#scope(accepted), discard_request_id: disposal.id }, { signal });
      if (signal.aborted || disposal !== this.#disposal || this.#retired || this.#busy) return;
      if (result.discard === "discarded" || result.discard === "retired") this.#retire();
      else {
        this.#notice = "Discard is unconfirmed.";
        this.#change();
      }
    } catch { /* The original request can still be retried explicitly. */ }
  }

  mount(pane: HTMLElement, feedback: HTMLElement, closed: AbortSignal, render: Render): void {
    const read = new AbortController();
    this.#read?.abort();
    this.#read = read;
    const signal = AbortSignal.any([closed, read.signal, this.connection]);
    let projection: Projection | undefined;
    let observing = false;
    let followTail = true;
    let readerNotice = "";
    let canRetryRead = false;
    let execution: Execution | undefined;
    let executionRead = false;
    let executionDirty = false;
    let executionError = false;
    let checkedStop: QuestionStop | undefined;
    let renderedTail: Position | null = null;
    const transcript = mount(ConversationPane, { target: pane, props: {
      loadEarlier: () => {}, refresh: () => { void observe(); }, reviewApproval: () => {},
      anchor: () => { followTail = false; },
    } });
    const discard = document.createElement("button");
    discard.type = "button";
    discard.textContent = "Discard";
    discard.title = "Discard this temporary question";
    discard.addEventListener("click", () => { void this.#discard(); }, { signal });
    const retryRead = document.createElement("button");
    retryRead.type = "button";
    retryRead.textContent = "Retry transcript";
    retryRead.addEventListener("click", () => { void observe(); }, { signal });
    const status = document.createElement("span");
    status.setAttribute("role", "status");
    const stop = document.createElement("button");
    stop.type = "button";
    stop.addEventListener("click", () => {
      if (this.#stop) {
        if (canRetryStop()) void this.#sendStop(this.#stop);
        else void inspectExecution();
      } else if (execution?.stoppable) {
        void this.#sendStop({ stop_request_id: crypto.randomUUID(), expected_revision: execution.revision, lease: execution.stoppable });
      }
    }, { signal });
    const controls = document.createElement("div");
    controls.className = "question-window-actions";
    controls.append(retryRead, stop, discard);
    feedback.classList.add("question-window-status");
    feedback.append(status, controls);
    const actions: ConversationConsole = {
      inbox: () => {}, browseModels: async () => undefined, editPreference: () => {},
      checkPreference: () => {}, retryPreference: () => {},
      edit: (text) => {
        this.#text = text;
        this.#edit++;
        if (this.#saveTimer !== undefined) clearTimeout(this.#saveTimer);
        this.#saveTimer = setTimeout(() => { this.flushDraft(); update(); }, 150);
        update();
      },
      send: () => { void this.#submit(); }, retry: () => { void this.#submit(true); },
      checkDelivery: () => { void this.#check(); },
      jumpLatest: () => { followTail = true; pane.scrollTop = pane.scrollHeight; update(); },
      addImages: (images) => {
        if (this.#pending || this.#busy) return;
        const attachments = { ...this.#attachments, images: [...(this.#attachments?.images ?? []), ...images] };
        if (!fitsImageSubmission(this.#text, attachments)) return imageSizeMessage;
        this.#attachments = attachments;
        this.#edit++;
        this.#change(true);
      },
      removeAttachment: (attachment) => {
        if (this.#pending || this.#busy || !this.#attachments) return;
        const quote = attachment === "quote" ? undefined : this.#attachments.quote;
        const images = this.#attachments.images?.filter((image) => image !== attachment);
        this.#attachments = quote === undefined && !images?.length ? undefined
          : { ...(quote === undefined ? {} : { quote }), ...(images?.length ? { images } : {}) };
        this.#edit++;
        this.#change(true);
      },
    };
    pane.addEventListener("scroll", () => {
      followTail = pane.scrollHeight - pane.scrollTop - pane.clientHeight <= 48;
      update();
    }, { signal });

    const draw = () => {
      const result = this.#tail && project({ flow: { session_id: this.flow.address.session_id, instance_id: this.#ids.question_id },
        temporary: this.#tail, items: this.#items }, projection);
      if (result) projection = result.state;
      flushSync(() => transcript.update({ rows: result ? result.rows : [], hasEarlier: false, loading: false,
        placeholder: this.#accepted ? "Waiting for the question’s transcript" : "Ask about this conversation" }));
      renderedTail = this.#tail;
      if (followTail) pane.scrollTop = pane.scrollHeight;
    };

    const update = () => {
      if (signal.aborted) return;
      if (renderedTail !== this.#tail) draw();
      status.textContent = readerNotice || (this.#retired ? "" : this.#stopNotice || (this.#stop ? "Checking stop…" : "Read-only reference"));
      retryRead.hidden = !canRetryRead || this.#retired;
      stop.hidden = !this.canStop || this.#retired || this.#disposal !== undefined || (!this.#stop && !execution?.stoppable);
      stop.disabled = this.#stopWriting || (!!this.#stop && (!!execution?.stop || (executionRead && !executionError)));
      const stopLabel = this.#stop ? (canRetryStop() ? "Retry stop" : "Check stop") : "Stop response";
      stop.title = stopLabel;
      stop.setAttribute("aria-label", stopLabel);
      stop.textContent = this.#stop && !stop.disabled ? stopLabel : "■";
      discard.hidden = !this.#accepted || this.#retired;
      discard.disabled = (!this.#tail && !this.#disposal) || this.#busy;
      discard.textContent = this.#disposal ? "Retry discard" : "Discard";
      render({ text: this.#text, feedback: this.#notice || (this.#saved && this.#saved !== "saved" ? "Some draft content is available only in this tab." : ""),
        editable: !this.#retired && !this.#disposal,
        canSend: this.#canSend(), canCheck: !!this.#pending && !this.#busy && !this.#disposal, canRetry: this.#retry && !this.#busy && !this.#disposal,
        hasPending: !!this.#pending, hasEarlierView: !followTail,
        ...(this.#attachments ? { attachments: this.#attachments } : {}) }, actions);
    };

    const canRetryStop = (): boolean => {
      const operation = this.#stop;
      const lease = execution?.stoppable;
      return !!operation && checkedStop === operation && !!lease && !executionError && !execution?.stop && execution?.revision === operation.expected_revision &&
        lease.turn === operation.lease.turn && lease.run === operation.lease.run && lease.epoch === operation.lease.epoch;
    };

    const inspectExecution = async (): Promise<void> => {
      const accepted = this.#accepted;
      if (!this.canStop || !accepted || this.#retired || this.#disposal || this.#stopWriting || signal.aborted) return;
      if (executionRead) {
        executionDirty = true;
        return;
      }
      const operation = this.#stop;
      executionRead = true;
      if (operation) update();
      try {
        const { result } = await this.flow.questionExecution({ ...this.#scope(accepted),
          ...(operation ? { stop_request_id: operation.stop_request_id } : {}) }, { signal });
        if (signal.aborted || this.#retired || this.#stop !== operation || this.#stopWriting) return;
        if (result.execution.state === "retired") { this.#retire(); return; }
        execution = result.execution.state === "observed" ? result.execution.observation : undefined;
        executionError = !execution;
        checkedStop = execution ? operation : undefined;
        if (!operation || !execution) return;
        if (execution.stop?.applied || (!execution.stop && execution.revision > operation.expected_revision)) {
          this.#stopNotice = execution.stop ? "Response stopped." : "That response has already ended.";
          this.#stop = undefined;
          this.flushDraft();
        } else this.#stopNotice = execution.stop ? "Stopping…" : "Stop is unconfirmed.";
      } catch {
        if (signal.aborted) return;
        execution = undefined;
        executionError = true;
        if (operation) this.#stopNotice = "Could not check stop.";
      } finally {
        executionRead = false;
        if (!signal.aborted) update();
        if (executionDirty) {
          executionDirty = false;
          void inspectExecution();
        }
      }
    };

    const observe = async () => {
      const accepted = this.#accepted;
      if (!accepted || observing || this.#retired || signal.aborted) return;
      observing = true;
      canRetryRead = false;
      let failures = 0;
      try {
        while (!signal.aborted && !this.#retired) {
          let response: Dtos["ReadQuestionTranscriptResponse"];
          try {
            ({ result: response } = await this.flow.questionTranscript({ ...this.#scope(accepted), after: this.#tail, limit: 32,
              wait: this.#tail !== null }, { signal }));
          } catch (error) {
            if (signal.aborted || !retryableReadFailure(error)) throw error;
            if (!await reconnect()) return;
            continue;
          }
          if (signal.aborted) return;
          const page = response.transcript;
          if (page.state === "retired") { this.#retire(); return; }
          if (page.state === "readers_busy") {
            if (!await reconnect()) return;
            continue;
          }
          if (page.state !== "ready") {
            readerNotice = page.state === "capacity" ? "This transcript exceeds the read limit." : "Transcript unavailable.";
            canRetryRead = page.state !== "capacity";
            return;
          }
          failures = 0;
          readerNotice = "";
          const previous = this.#tail;
          const count = this.#items.length;
          if (page.batches.length) {
            for (const batch of page.batches) this.#items.push(...batch.items);
            this.#tail = page.next ?? null;
          }
          try {
            update();
          } catch (error) {
            this.#items.length = count;
            this.#tail = previous;
            throw error;
          }
          if (this.#tail !== previous) void inspectExecution();
        }
      } catch (error) {
        if (!signal.aborted) {
          readerNotice = error instanceof Error ? error.message : "Could not read the transcript.";
          canRetryRead = true;
        }
      } finally {
        observing = false;
        if (!signal.aborted) update();
      }

      function reconnect(): Promise<boolean> {
        readerNotice = "Reconnecting to the question…";
        update();
        const delay = Math.min(8000, 250 * 2 ** failures);
        failures = Math.min(failures + 1, 5);
        return waitForReadRetry(delay, signal);
      }
    };

    this.#changes.addEventListener("change", () => { update(); void observe(); void inspectExecution(); }, { signal });
    signal.addEventListener("abort", () => { void unmount(transcript); }, { once: true });
    draw();
    update();
    void observe();
    void inspectExecution();
    if (this.#disposal) void this.#checkDiscard(signal);
    else if (this.#pending && !this.#busy) void this.#check();
  }
}
