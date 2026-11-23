import { flushSync, mount, unmount } from "svelte";
import type { Dtos, FlowClient } from "../src/index.ts";
import ConversationPane from "./ConversationPane.svelte";
import { fitsImageSubmission, imageSizeMessage } from "./composer-images.ts";
import type { ConsoleSnapshot, ConversationConsole } from "./conversation.ts";
import { project, type State as Projection } from "./live-projection.ts";
import type { DraftAttachments } from "./tab-drafts.ts";

export const questionMethods = ["question.status", "question.start", "question.transcript",
  "question.followup", "question.followup_status", "question.discard", "question.discard_status"];

type Page = Dtos["ConversationPage"];
type Position = Dtos["QuestionPosition"];
type Send = { id: string; edit: number; user: Dtos["SubmittedUserContent"]; observed?: Position };
type Accepted = { owner: string; receipt: Dtos["QuestionStartReceipt"] };
type Render = (snapshot: ConsoleSnapshot, actions: ConversationConsole) => void;

export class QuestionWindow {
  readonly #ids = { question_id: crypto.randomUUID(), request_id: crypto.randomUUID() };
  readonly #changes = new EventTarget();
  readonly #items: Dtos["ConversationItem"][] = [];
  #owner: string | undefined;
  #accepted: Accepted | undefined;
  #tail: Position | null = null;
  #text = "";
  #edit = 0;
  #attachments: DraftAttachments | undefined;
  #pending: Send | undefined;
  #busy = false;
  #retry = false;
  #retired = false;
  #notice = "";
  #read: AbortController | undefined;
  #disposal: { id: string; observed: Position } | undefined;

  constructor(readonly flow: FlowClient, readonly source: Pick<Page, "head" | "source">,
    readonly connection: AbortSignal) {}

  get remains(): boolean { return !this.#retired && (this.#accepted !== undefined || this.#pending !== undefined); }

  #change(): void { this.#changes.dispatchEvent(new Event("change")); }

  #scope(accepted: Accepted) {
    return { ...this.#ids, owner_generation: accepted.owner, host_generation: accepted.receipt.launch.host_generation };
  }

  #accept(pending: Send): void {
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
    if (retry ? !this.#pending || !this.#retry : !this.#canSend()) return;
    const pending = this.#pending ?? { id: this.#accepted ? crypto.randomUUID() : this.#ids.request_id,
      user: { text: this.#text, ...this.#attachments }, edit: this.#edit, ...(this.#tail ? { observed: this.#tail } : {}) };
    this.#pending = pending;
    this.#retry = false;
    this.#busy = true;
    this.#notice = "Sending…";
    this.#change();
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
      this.#change();
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
      this.#change();
    }
  }

  #retire(): void {
    this.#retired = true;
    this.#notice = "This question has ended.";
    this.#pending = undefined;
    this.#change();
  }

  #unconfirmed(retry: boolean): void {
    this.#retry = retry;
    this.#notice = "Delivery is unconfirmed.";
  }

  #canSend(): boolean {
    return !this.#retired && !this.connection.aborted && !this.#disposal && !this.#busy && !this.#pending
      && (!this.#accepted || this.#tail !== null) && Boolean(this.#text.trim() || this.#attachments);
  }

  async #discard(): Promise<void> {
    if (!this.#accepted || !this.#tail || this.#busy || this.#retired || this.connection.aborted) return;
    const operation = this.#disposal ??= { id: crypto.randomUUID(), observed: this.#tail };
    const scope = { ...this.#scope(this.#accepted), discard_request_id: operation.id };
    const options = { signal: this.connection };
    this.#busy = true;
    this.#notice = "Discarding…";
    this.#change();
    try {
      const { result } = await this.flow.questionDiscardStatus(scope, options);
      if (result.discard !== "discarded" && result.discard !== "retired")
        await this.flow.questionDiscard({ ...scope, observed: operation.observed }, options);
      this.#retire();
    } catch (error) {
      this.#notice = error instanceof Error ? error.message : "Could not confirm discard.";
    } finally {
      this.#busy = false;
      this.#change();
    }
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
    const status = document.createElement("span");
    feedback.classList.add("question-window-status");
    feedback.append(status, discard);
    const actions: ConversationConsole = {
      inbox: () => {}, browseModels: async () => undefined, editPreference: () => {},
      checkPreference: () => {}, retryPreference: () => {},
      edit: (text) => { this.#text = text; this.#edit++; update(); },
      send: () => { void this.#submit(); }, retry: () => { void this.#submit(true); },
      checkDelivery: () => { void this.#check(); },
      jumpLatest: () => { followTail = true; pane.scrollTop = pane.scrollHeight; update(); },
      addImages: (images) => {
        if (this.#pending || this.#busy) return;
        const attachments = { ...this.#attachments, images: [...(this.#attachments?.images ?? []), ...images] };
        if (!fitsImageSubmission(this.#text, attachments)) return imageSizeMessage;
        this.#attachments = attachments;
        this.#edit++;
        this.#change();
      },
      removeAttachment: (attachment) => {
        if (this.#pending || this.#busy || !this.#attachments) return;
        const quote = attachment === "quote" ? undefined : this.#attachments.quote;
        const images = this.#attachments.images?.filter((image) => image !== attachment);
        this.#attachments = quote === undefined && !images?.length ? undefined
          : { ...(quote === undefined ? {} : { quote }), ...(images?.length ? { images } : {}) };
        this.#edit++;
        this.#change();
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
      status.textContent = readerNotice || (this.#retired ? "" : "Read-only reference");
      discard.hidden = !this.#accepted || this.#retired;
      discard.disabled = !this.#tail || this.#busy;
      render({ text: this.#text, feedback: this.#notice, editable: !this.#retired && !this.#disposal,
        canSend: this.#canSend(), canCheck: !!this.#pending && !this.#busy, canRetry: this.#retry && !this.#busy,
        hasPending: !!this.#pending, hasEarlierView: !followTail,
        ...(this.#attachments ? { attachments: this.#attachments } : {}) }, actions);
    };

    const observe = async () => {
      const accepted = this.#accepted;
      if (!accepted || observing || this.#retired || signal.aborted) return;
      observing = true;
      try {
        while (!signal.aborted && !this.#retired) {
          const { result } = await this.flow.questionTranscript({ ...this.#scope(accepted), after: this.#tail, limit: 32,
            wait: this.#tail !== null }, { signal });
          if (signal.aborted) return;
          if (result.transcript.state === "retired") { this.#retire(); return; }
          if (result.transcript.state !== "ready") {
            readerNotice = result.transcript.state === "capacity" ? "This transcript exceeds the read limit." : "Transcript unavailable.";
            return;
          }
          readerNotice = "";
          if (result.transcript.batches.length) {
            for (const batch of result.transcript.batches) this.#items.push(...batch.items);
            this.#tail = result.transcript.next ?? null;
          }
          update();
        }
      } catch (error) {
        if (!signal.aborted) readerNotice = error instanceof Error ? error.message : "Could not read the transcript.";
      } finally {
        observing = false;
        if (!signal.aborted) update();
      }
    };

    this.#changes.addEventListener("change", () => { update(); void observe(); }, { signal });
    signal.addEventListener("abort", () => { void unmount(transcript); }, { once: true });
    draw();
    update();
    void observe();
    if (this.#pending && !this.#busy) void this.#check();
  }
}
