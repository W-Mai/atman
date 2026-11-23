import { flushSync, mount, unmount } from "svelte";
import { ClientError, RemoteError, cursorId } from "../src/index.ts";
import type { Dtos, FlowClient } from "../src/index.ts";
import ConversationPane from "./ConversationPane.svelte";
import { fitsImageSubmission, imageSizeMessage } from "./composer-images.ts";
import { questionEffort, type ConsoleSettings } from "./composer-settings.ts";
import type { ConsoleSnapshot, ConversationConsole } from "./conversation.ts";
import { retryableReadFailure, waitForReadRetry } from "./durable-observer.ts";
import { project, type State as Projection } from "./live-projection.ts";
import { samePosition } from "./question-drafts.ts";
import type { QuestionDisposal, QuestionDraft, QuestionDrafts, QuestionKeep, QuestionModelSelection, QuestionRefresh, QuestionSend, QuestionSource, QuestionStop } from "./question-drafts.ts";
import type { DraftAttachments, DraftBinding, DraftContent, PersistDraftResult } from "./tab-drafts.ts";

export const questionMethods = ["question.status", "question.start", "question.transcript",
  "question.followup", "question.followup_status", "question.discard", "question.discard_status"];
export const questionStopMethods = ["question.execution", "question.stop"];
export const questionRefreshMethods = ["question.execution", "question.refresh", "question.refresh_status"];
export const questionKeepMethods = ["question.execution", "question.keep", "question.keep_status", "question.keep_installed", "flow.events"];
export const questionModelMethods = ["question.model_options", "question.model_check"];
export const questionLiveMethod = "question.observe";

type Position = Dtos["QuestionPosition"];
type DraftSnapshot = Dtos["QuestionDraftSnapshot"];
type Execution = Dtos["QuestionExecutionObservation"];
type Accepted = { owner: string; receipt: Dtos["QuestionStartReceipt"] };
type Render = (snapshot: ConsoleSnapshot, actions: ConversationConsole) => void;
type Archive = { drafts: QuestionDrafts; binding: DraftBinding };
type Controls = Readonly<{ stop: boolean; refresh: boolean; keep: boolean; models: boolean; live: boolean }>;
type KeepReceipt = Dtos["QuestionKeepReceipt"];
type ReadingPosition = Readonly<{ scrollTop: number; followingTail: boolean }>;
type InstallSavedFlow = (receipt: KeepReceipt, draft: DraftContent, reading: () => ReadingPosition) => Promise<boolean>;

export class QuestionWindow {
  readonly #ids: QuestionDraft["ids"];
  readonly source: QuestionSource;
  readonly #changes = new EventTarget();
  readonly #items: Dtos["ConversationItem"][] = [];
  #checkpoints: Position[] = [];
  #drafts: DraftSnapshot | undefined;
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
  #refresh: QuestionRefresh | undefined;
  #refreshChecking = false;
  #refreshUnseen = false;
  #referenceNotice = "";
  #keep: QuestionKeep | undefined;
  #keeping = false;
  #keepRetry = false;
  #keepNotice = "";
  #saveTimer: ReturnType<typeof setTimeout> | undefined;
  #saved: PersistDraftResult | undefined;
  #selection: QuestionModelSelection;

  constructor(readonly flow: FlowClient, source: QuestionSource, readonly connection: AbortSignal,
    readonly archive: Archive | undefined, readonly controls: Controls) {
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
    this.#refresh = restored?.refresh;
    this.#keep = restored?.keep;
    this.#selection = restored?.selection ?? { model: "smart" };
  }

  get remains(): boolean { return !this.#retired && (this.#accepted !== undefined || this.#pending !== undefined); }
  get retired(): boolean { return this.#retired; }

  flushDraft(): void {
    if (this.#saveTimer !== undefined) clearTimeout(this.#saveTimer);
    this.#saveTimer = undefined;
    if (!this.archive) return;
    const { drafts, binding } = this.archive;
    if (this.#retired || (!this.remains && !this.#text && !this.#attachments && this.#selection.model === "smart" && !this.#selection.reasoning)) {
      drafts.remove(binding);
      return;
    }
    this.#saved = drafts.save(binding, {
      anchor: this.source, ids: this.#ids, edit: this.#edit, text: this.#text,
      ...(this.#owner ? { owner: this.#owner } : {}), ...(this.#accepted ? { receipt: this.#accepted.receipt } : {}),
      ...(this.#attachments ? { attachments: this.#attachments } : {}), ...(this.#pending ? { pending: this.#pending } : {}),
      ...(this.#disposal ? { disposal: this.#disposal } : {}),
      ...(this.#stop ? { stop: this.#stop } : {}),
      ...(this.#refresh ? { refresh: this.#refresh } : {}),
      ...(this.#keep ? { keep: this.#keep } : {}),
      selection: this.#selection,
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

  #invocation() {
    const effort = questionEffort(this.#selection.reasoning);
    return { model: this.#selection.model, ...(effort ? { effort } : {}) };
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
    if (this.#busy || this.#retired || this.connection.aborted || this.#disposal || this.#keep) return;
    if (retry ? !this.#pending?.user || !this.#retry : !this.#canSend()) return;
    const pending = this.#pending ?? { id: this.#accepted ? crypto.randomUUID() : this.#ids.request_id,
      user: { text: this.#text, ...this.#attachments }, edit: this.#edit, ...this.#invocation(), ...(this.#tail ? { observed: this.#tail } : {}) };
    if (!pending.user) return;
    this.#pending = pending;
    this.#stopNotice = "";
    if (!this.#refresh) this.#referenceNotice = "";
    this.#retry = false;
    this.#busy = true;
    this.#notice = "Sending…";
    this.#change(true);
    try {
      const options = { signal: this.connection };
      const invocation = { model: pending.model ?? "smart", ...(pending.effort ? { effort: pending.effort } : {}) };
      if (pending.observed && this.#accepted) {
        await this.flow.questionFollowup({ ...this.#scope(this.#accepted), followup_request_id: pending.id,
          observed: pending.observed, user: pending.user, ...invocation }, options);
      } else {
        if (this.#owner === undefined) {
          const { result } = await this.flow.questionStatus(this.#ids, options);
          if (result.owner_generation == null) throw new Error("Open the source Flow to ask a question.");
          this.#owner = result.owner_generation;
          this.flushDraft();
        }
        const { result } = await this.flow.startQuestion({ ...this.#ids, owner_generation: this.#owner,
          observed: this.source.source, expected_head: this.source.head, user: pending.user, ...invocation }, options);
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
    this.#keepNotice = "";
    this.#notice = "This question has ended.";
    this.#pending = undefined;
    this.#change(true);
  }

  #unconfirmed(retry: boolean): void {
    this.#retry = retry && this.#pending?.user !== undefined;
    this.#notice = this.#pending?.user ? "Delivery is unconfirmed." : "Only the request identity is available for delivery lookup.";
  }

  #canSend(): boolean {
    return !this.#retired && !this.connection.aborted && !this.#disposal && !this.#busy && !this.#pending && !this.#keep
      && (!this.#accepted || this.#tail !== null) && Boolean(this.#text.trim() || this.#attachments);
  }

  async #sendStop(operation: QuestionStop): Promise<void> {
    const accepted = this.#accepted;
    if (!accepted || this.#stopWriting || this.#retired || this.#disposal || this.#keep || this.connection.aborted) return;
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

  #confirmRefresh(operation: QuestionRefresh, receipt: Dtos["QuestionRefreshReceipt"]): void {
    const before = operation.observed_source.cursor;
    const after = receipt.source.cursor;
    if (receipt.source.branch !== operation.source_head.branch || receipt.applied.sequence <= operation.expected.sequence ||
      after.sequence < before.sequence || (after.sequence === before.sequence && after.digest !== before.digest))
      throw new Error("Reference receipt differs from the original refresh.");
    this.#refresh = undefined;
    this.#referenceNotice = "Reference refreshed.";
  }

  async #checkRefresh(): Promise<void> {
    const accepted = this.#accepted;
    const operation = this.#refresh;
    if (!this.controls.refresh || !accepted || !operation || this.#busy || this.#retired || this.#disposal ||
      this.#refreshChecking || this.connection.aborted) return;
    const observed = this.#tail;
    this.#refreshChecking = true;
    this.#refreshUnseen = false;
    this.#referenceNotice = "Checking reference…";
    this.#change();
    try {
      const { result } = await this.flow.questionRefreshStatus({ ...this.#scope(accepted),
        refresh_request_id: operation.refresh_request_id }, { signal: this.connection });
      if (this.#refresh !== operation || this.#retired || this.#disposal) return;
      const state = result.refresh;
      if (state.state === "applied") this.#confirmRefresh(operation, state.receipt);
      else if (state.state === "retired") this.#retire();
      else if (state.state === "unseen" && observed && observed.sequence > operation.expected.sequence) {
        this.#refresh = undefined;
        this.#referenceNotice = "The question changed before the reference could refresh.";
      } else {
        this.#refreshUnseen = state.state === "unseen";
        this.#referenceNotice = "Reference refresh is unconfirmed.";
      }
    } catch {
      this.#referenceNotice = "Could not check the reference refresh.";
    } finally {
      this.#refreshChecking = false;
      this.#change(true);
      if (this.#refresh === operation && this.#tail !== observed && this.#tail && this.#tail.sequence > operation.expected.sequence)
        void this.#checkRefresh();
    }
  }

  async #refreshReference(observed: Execution): Promise<void> {
    const accepted = this.#accepted;
    const boundary = this.#tail;
    if (!accepted || !boundary || this.#retired || this.#disposal || this.#busy || this.#pending || this.#stop || this.#keep ||
      this.#refreshChecking || this.connection.aborted || observed.phase !== "idle" || !samePosition(observed.cursor, boundary)) return;
    let operation = this.#refresh;
    if (operation && (!this.#refreshUnseen || !samePosition(boundary, operation.expected))) return;
    this.#busy = true;
    const invocation = this.#invocation();
    this.#stopNotice = "";
    this.#referenceNotice = "Refreshing reference…";
    this.#change();
    try {
      const options = { signal: this.connection };
      if (!operation) {
        const { result } = await this.flow.conversation({ limit: 1 }, options);
        if (this.#retired || this.connection.aborted) return;
        if (!samePosition(this.#tail, boundary)) throw new Error("The question changed before the reference could refresh.");
        if (result.state !== "ready") throw new Error("Could not read the source conversation.");
        const { head, source } = result.page;
        if (head.branch !== this.source.head.branch || source.branch !== this.source.source.branch)
          throw new Error("Open the original branch to refresh this reference.");
        operation = { refresh_request_id: crypto.randomUUID(), expected: boundary, source_head: head,
          observed_source: source, ...invocation };
        this.#refresh = operation;
        this.flushDraft();
      }
      const { result } = await this.flow.questionRefresh({ ...this.#scope(accepted), ...operation }, options);
      this.#confirmRefresh(operation, result.receipt);
    } catch (error) {
      this.#referenceNotice = error instanceof Error ? error.message : "Could not refresh the reference.";
    } finally {
      this.#busy = false;
      this.#change(true);
      if (operation && this.#refresh === operation) void this.#checkRefresh();
    }
  }

  async #discard(): Promise<void> {
    const observed = this.#tail ?? this.#disposal?.observed;
    if (!this.#accepted || !observed || this.#busy || this.#retired || this.#keep || this.connection.aborted) return;
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

  async #resolveKeep(write: boolean, view: AbortSignal, install: InstallSavedFlow, reading: () => ReadingPosition): Promise<void> {
    const accepted = this.#accepted;
    if (!accepted || !this.controls.keep || this.#keeping || this.#retired || this.#busy || this.#disposal || this.#pending ||
      this.#stop || this.#stopWriting || this.#refresh || this.#refreshChecking || view.aborted) return;
    if (!this.#keep && (!write || !this.#tail)) return;
    let operation = this.#keep ??= { keep_request_id: crypto.randomUUID(), expected: this.#tail! };
    const lookup = { ...this.#scope(accepted), keep_request_id: operation.keep_request_id };
    const options = { signal: this.connection };
    const status = async () => (await this.flow.questionKeepStatus(lookup, options)).result.keep;
    this.#keeping = true;
    this.#keepRetry = false;
    this.#keepNotice = write ? "Saving question…" : "Checking saved question…";
    this.#change(true);
    try {
      let state: Dtos["QuestionKeepState"] | undefined;
      let failure: unknown;
      if (write && !operation.publication) {
        try {
          state = (await this.flow.questionKeep({ ...lookup, expected: operation.expected }, options)).result.keep;
        } catch (error) { failure = error; }
      }
      if (!state) {
        try { state = await status(); }
        catch (error) { if (!operation.publication) throw error; }
      }
      let publication = operation.publication;
      if (state?.state === "published" || state?.state === "installed") {
        if (!samePosition(state.receipt.source, operation.expected) ||
          (publication && cursorId(publication.destination) !== cursorId(state.receipt.destination)))
          throw new Error("Saved Flow differs from the original question.");
        publication = state.receipt;
        operation = { ...operation, publication };
        this.#keep = operation;
        this.flushDraft();
      }
      if (!publication) {
        if (state?.state === "retired") { this.#retire(); return; }
        if (state?.state === "unseen" && (failure instanceof RemoteError ||
          (failure instanceof ClientError && failure.outcome === "not_sent"))) {
          this.#keep = undefined;
          this.#keepNotice = failure.message;
        } else {
          this.#keepRetry = state?.state === "unseen" || state?.state === "pending";
          this.#keepNotice = "Save is unconfirmed.";
        }
        return;
      }
      this.#keepNotice = "Opening saved Flow…";
      this.#change();
      const draft: DraftContent = { text: this.#text, ...(this.#attachments ? { attachments: this.#attachments } : {}) };
      if (view.aborted || !await install(publication, draft, reading)) {
        this.#keepNotice = "Question saved. Open the saved Flow.";
        return;
      }
      this.#text = "";
      this.#attachments = undefined;
      this.flushDraft();
      if (state?.state !== "installed" && state?.state !== "retired") {
        const installed = publication.destination;
        for (const delay of [0, 300, 900]) {
          if (this.connection.aborted || (delay && !await waitForReadRetry(delay, this.connection))) return;
          try {
            await this.flow.questionKeepInstalled({ ...lookup, installed }, options);
            this.#retire();
            return;
          } catch {
            const latest = await status().catch(() => undefined);
            if (latest?.state === "retired" || (latest?.state === "installed" && samePosition(latest.receipt.source, operation.expected) &&
              cursorId(latest.receipt.destination) === cursorId(installed))) break;
            if (delay === 900) return;
          }
        }
      }
      this.#retire();
    } catch (error) {
      this.#keepNotice = error instanceof Error ? error.message : "Could not open the saved Flow.";
    } finally {
      this.#keeping = false;
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

  mount(pane: HTMLElement, feedback: HTMLElement, closed: AbortSignal, render: Render,
    bring: (answer: string, available: () => boolean) => Promise<string | undefined>, install: InstallSavedFlow): void {
    const read = new AbortController();
    this.#read?.abort();
    this.#read = read;
    const signal = AbortSignal.any([closed, read.signal, this.connection]);
    let projection: Projection | undefined;
    let observing = false;
    let followTail = true;
    let resumeKeep = !!this.#keep;
    let readerNotice = "";
    let bringNotice = "";
    let bringing = false;
    let canRetryRead = false;
    let execution: Execution | undefined;
    let executionRead = false;
    let executionDirty = false;
    let executionError = false;
    let checkedStop: QuestionStop | undefined;
    let renderedTail: Position | null = null;
    let renderedDrafts: DraftSnapshot | undefined;
    let modelPreview: Dtos["ComposerModelState"] = { status: "pending" };
    let modelRevision = crypto.randomUUID();
    let modelReading = false;
    let modelOwner: Accepted | undefined;
    let selectingModel = false;
    let modelNotice = "";
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
    const refreshReference = document.createElement("button");
    refreshReference.type = "button";
    refreshReference.textContent = "↻";
    refreshReference.addEventListener("click", () => {
      if (execution && mayRefreshReference()) void this.#refreshReference(execution);
      else void this.#checkRefresh();
    }, { signal });
    const keep = document.createElement("button");
    keep.type = "button";
    keep.textContent = "◇";
    const reading = () => ({ scrollTop: pane.scrollTop, followingTail: followTail });
    const observeKeep = () => { void this.#resolveKeep(false, signal, install, reading); };
    keep.addEventListener("click", () => {
      if (this.#keep ? this.#keepRetry : canKeep()) void this.#resolveKeep(true, signal, install, reading);
      else if (this.#keep) observeKeep();
    }, { signal });
    const controls = document.createElement("div");
    controls.className = "question-window-actions";
    controls.append(retryRead, refreshReference, stop, keep, discard);
    feedback.classList.add("question-window-status");
    feedback.append(status, controls);

    const projectModels = (result?: Dtos["QuestionModelOptionsResponse"]): ConsoleSettings => {
      const model = result?.model ?? modelPreview;
      return { model, question: true,
        edit_revisions: { model: modelRevision, ...(model.status === "ready" && model.reasoning.available ? { reasoning: modelRevision } : {}) },
        ...(result?.catalog ? { catalog: result.catalog } : {}) };
    };

    const readModels = async (choice: QuestionModelSelection, owner: Accepted | undefined, signal: AbortSignal, query?: string) => {
      const { result } = await this.flow.questionModelOptions({ model: choice.model,
        ...(choice.reasoning ? { reasoning: choice.reasoning } : {}), ...(owner ? { question: this.#scope(owner) } : {}),
        ...(query === undefined ? {} : { catalog_query: query }) }, { signal });
      return result;
    };

    const showSelectedModel = async () => {
      const owner = this.#accepted;
      const choice = this.#selection;
      if (!this.controls.models || this.#retired || signal.aborted || (modelReading && modelOwner === owner)) return;
      modelReading = true;
      modelOwner = owner;
      try {
        const result = await readModels(choice, owner, signal);
        if (signal.aborted || this.#accepted !== owner || this.#selection !== choice || selectingModel) return;
        modelPreview = result.model;
      } catch {
        if (signal.aborted || this.#accepted !== owner || this.#selection !== choice || selectingModel) return;
        modelPreview = { status: "unavailable", reason: "configuration_unavailable" };
      }
      update();
    };

    const chooseModel = async (revision: string, edit: Dtos["ComposerPreferenceEdit"]) => {
      if (!this.controls.models || revision !== modelRevision || signal.aborted || selectingModel || this.#busy || this.#keep || this.#retired || this.#disposal) return;
      if (edit.field !== "model" && edit.field !== "reasoning") return;
      const current = this.#selection;
      const reason = edit.field === "model" ? current.reasoning : edit.value.kind === "selection" ? edit.value.selection : undefined;
      const choice = { model: edit.field === "model" ? edit.key : current.model, ...(reason ? { reasoning: reason } : {}) };
      const owner = this.#accepted;
      const boundary = this.#tail;
      selectingModel = true;
      modelNotice = "Checking model…";
      update();
      try {
        if ((owner && !boundary) || (this.#pending && !owner)) throw new Error("Wait for the first question to be confirmed.");
        const { model } = await readModels(choice, owner, signal);
        if (model.status !== "ready" || !model.context_budget) throw new Error("This model has no available context budget.");
        if (reason && (!model.reasoning.available || model.reasoning.error)) throw new Error(model.reasoning.error || "Reasoning is unavailable for this model.");
        if (owner && boundary) {
          const effort = questionEffort(reason);
          const { result } = await this.flow.questionModelCheck({ ...this.#scope(owner), expected: boundary, model: model.model,
            ...(effort ? { effort } : {}) }, { signal });
          if (result.model !== model.model || !samePosition(this.#tail, boundary)) throw new Error("The question changed. Select the model again.");
        }
        if (signal.aborted || this.#accepted !== owner || this.#keep || this.#disposal || this.#retired) return;
        this.#selection = { ...choice, model: model.model };
        modelPreview = { ...model, key: model.model };
        modelRevision = crypto.randomUUID();
        modelNotice = "";
        this.flushDraft();
      } catch (error) {
        if (!signal.aborted) modelNotice = error instanceof Error ? error.message : "Could not select the model.";
      } finally {
        selectingModel = false;
        update();
      }
    };

    const actions: ConversationConsole = {
      inbox: () => {},
      browseModels: async (query, cancelled) => {
        if (!this.controls.models) return;
        const selection = this.#selection;
        const owner = this.#accepted;
        const request = AbortSignal.any([cancelled, signal]);
        const result = await readModels(selection, owner, request, query);
        if (request.aborted || selection !== this.#selection || owner !== this.#accepted) return;
        if (!selectingModel) { modelPreview = result.model; update(); }
        return projectModels(result);
      },
      editPreference: (revision, selection) => { void chooseModel(revision, selection); },
      checkPreference: () => {}, retryPreference: () => {},
      edit: (text) => {
        if (this.#keep) return;
        this.#text = text;
        this.#edit++;
        if (this.#saveTimer !== undefined) clearTimeout(this.#saveTimer);
        this.#saveTimer = setTimeout(() => { this.flushDraft(); update(); }, 150);
        update();
      },
      send: () => { if (!selectingModel) void this.#submit(); }, retry: () => { if (!selectingModel) void this.#submit(true); },
      checkDelivery: () => { void this.#check(); },
      jumpLatest: () => { followTail = true; pane.scrollTop = pane.scrollHeight; update(); },
      addImages: (images) => {
        if (this.#pending || this.#busy || this.#keep) return;
        const attachments = { ...this.#attachments, images: [...(this.#attachments?.images ?? []), ...images] };
        if (!fitsImageSubmission(this.#text, attachments)) return imageSizeMessage;
        this.#attachments = attachments;
        this.#edit++;
        this.#change(true);
      },
      removeAttachment: (attachment) => {
        if (this.#pending || this.#busy || this.#keep || !this.#attachments) return;
        const quote = attachment === "quote" ? undefined : this.#attachments.quote;
        const images = this.#attachments.images?.filter((image) => image !== attachment);
        this.#attachments = quote === undefined && !images?.length ? undefined
          : { ...(quote === undefined ? {} : { quote }), ...(images?.length ? { images } : {}) };
        this.#edit++;
        this.#change(true);
      },
      editQuote: (original, text) => {
        if (signal.aborted || this.#retired || this.#disposal || this.#keep || this.#pending || this.#busy || this.#attachments?.quote !== original)
          return "The quote changed. Reopen it to edit.";
        const images = this.#attachments.images;
        const attachments = { ...(images?.length ? { images } : {}), ...(text.trim() ? { quote: text } : {}) };
        if (!fitsImageSubmission(this.#text, attachments)) return imageSizeMessage;
        this.#attachments = attachments.quote !== undefined || images?.length ? attachments : undefined;
        this.#edit++;
        this.#change(true);
      },
    };
    pane.addEventListener("scroll", () => {
      followTail = pane.scrollHeight - pane.scrollTop - pane.clientHeight <= 48;
      update();
    }, { signal });

    const bringAnswer = async (answer: string) => {
      const available = () => !signal.aborted && !this.#retired && !this.#disposal;
      if (bringing || !available()) return;
      bringing = true;
      bringNotice = "Checking source…";
      update();
      try {
        bringNotice = await bring(answer, available) ?? "";
      } catch (error) {
        bringNotice = error instanceof Error ? error.message : "Could not check the source.";
      } finally {
        bringing = false;
        update();
      }
    };

    const draw = () => {
      const result = this.#tail && project({ flow: { session_id: this.flow.address.session_id, instance_id: this.#ids.question_id },
        temporary: this.#tail, items: this.#items, snapshot: this.#drafts, verifiedCheckpoints: this.#checkpoints }, projection);
      if (result) projection = result.state;
      flushSync(() => transcript.update({ rows: result ? result.rows : [], hasEarlier: false, loading: false,
        placeholder: this.#accepted ? "Waiting for the question’s transcript" : "Ask about this conversation",
        bring: (answer) => { void bringAnswer(answer); } }));
      renderedTail = this.#tail;
      renderedDrafts = this.#drafts;
      if (followTail) pane.scrollTop = pane.scrollHeight;
    };

    const update = () => {
      if (signal.aborted) return;
      if (renderedTail !== this.#tail || renderedDrafts !== this.#drafts) draw();
      status.textContent = this.#keepNotice || readerNotice || (this.#retired ? "" : bringNotice || this.#stopNotice || this.#referenceNotice ||
        (this.#stop ? "Checking stop…" : this.#refresh ? "Checking reference…" : "Read-only reference"));
      retryRead.hidden = !canRetryRead || this.#retired;
      stop.hidden = !this.controls.stop || this.#retired || !!this.#keep || this.#disposal !== undefined || (!this.#stop && !execution?.stoppable);
      stop.disabled = this.#stopWriting || (!!this.#stop && (!!execution?.stop || (executionRead && !executionError)));
      const stopLabel = this.#stop ? (canRetryStop() ? "Retry stop" : "Check stop") : "Stop response";
      stop.title = stopLabel;
      stop.setAttribute("aria-label", stopLabel);
      stop.textContent = this.#stop && !stop.disabled ? stopLabel : "■";
      refreshReference.hidden = !this.controls.refresh || !this.#accepted || this.#retired || !!this.#disposal || !!this.#keep;
      refreshReference.disabled = this.#busy || this.#refreshChecking || (!this.#refresh && !mayRefreshReference());
      const referenceLabel = this.#refresh ? (mayRefreshReference() ? "Retry reference refresh" : "Check reference refresh") : "Refresh reference";
      refreshReference.title = referenceLabel;
      refreshReference.setAttribute("aria-label", referenceLabel);
      keep.hidden = !this.controls.keep || !this.#accepted || this.#retired || !!this.#disposal;
      keep.disabled = this.#keeping || (!this.#keep && !canKeep());
      const keepLabel = !this.#keep ? "Keep question" : this.#keep.publication ? "Open saved Flow" : this.#keepRetry ? "Retry Keep" : "Check Keep";
      keep.title = keepLabel;
      keep.setAttribute("aria-label", keepLabel);
      discard.hidden = !this.#accepted || this.#retired || !!this.#keep;
      discard.disabled = (!this.#tail && !this.#disposal) || this.#busy;
      discard.textContent = this.#disposal ? "Retry discard" : "Discard";
      render({ text: this.#text, feedback: this.#notice || (this.#saved && this.#saved !== "saved" ? "Some draft content is available only in this tab." : ""),
        editable: !this.#retired && !this.#disposal && !this.#keep,
        canSend: !selectingModel && this.#canSend(), canCheck: !!this.#pending && !this.#busy && !this.#disposal,
        canRetry: !selectingModel && this.#retry && !this.#busy && !this.#disposal,
        hasPending: !!this.#pending, hasEarlierView: !followTail,
        ...(this.controls.models ? { settings: projectModels(), preference: { busy: selectingModel || this.#busy || this.#retired || !!this.#keep || !!this.#disposal,
          unsettled: false, retryable: false, message: modelNotice } } : {}),
        ...(this.#attachments ? { attachments: this.#attachments } : {}) }, actions);
    };

    const canRetryStop = (): boolean => {
      const operation = this.#stop;
      const lease = execution?.stoppable;
      return !!operation && checkedStop === operation && !!lease && !executionError && !execution?.stop && execution?.revision === operation.expected_revision &&
        lease.turn === operation.lease.turn && lease.run === operation.lease.run && lease.epoch === operation.lease.epoch;
    };

    const canKeep = (): boolean => {
      return !selectingModel && !this.#busy && !this.#pending && !this.#stop && !this.#refresh && !this.#refreshChecking && !!this.#tail &&
        execution?.phase === "idle" && samePosition(execution.cursor, this.#tail);
    };

    const mayRefreshReference = (): boolean => {
      const boundary = this.#tail;
      return !selectingModel && !!boundary && !this.#stop && !this.#pending && execution?.phase === "idle" && samePosition(execution.cursor, boundary) &&
        (!this.#refresh || (this.#refreshUnseen && samePosition(boundary, this.#refresh.expected)));
    };

    const inspectExecution = async (): Promise<void> => {
      const accepted = this.#accepted;
      if ((!this.controls.stop && !this.controls.refresh && !this.controls.keep) || !accepted || this.#retired || this.#disposal ||
        this.#keep || this.#stopWriting || signal.aborted) return;
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
        if (signal.aborted || this.#retired || this.#keep || this.#stop !== operation || this.#stopWriting) return;
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
          let drafts: DraftSnapshot | undefined;
          try {
            const page = { ...this.#scope(accepted), after: this.#tail, limit: 32, wait: this.#tail !== null };
            if (this.controls.live) {
              const { result } = await this.flow.questionObserve({ ...page, known: this.#drafts?.edition ?? null }, { signal });
              response = result.page;
              drafts = result.drafts ?? undefined;
            } else {
              ({ result: response } = await this.flow.questionTranscript(page, { signal }));
            }
          } catch (error) {
            if (signal.aborted || !retryableReadFailure(error)) throw error;
            if (!await reconnect()) return;
            continue;
          }
          if (signal.aborted) return;
          const page = response.transcript;
          if (page.state === "retired") {
            if (!this.#keep) this.#retire();
            return;
          }
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
          const previousDrafts = this.#drafts;
          const previousCheckpoints = this.#checkpoints;
          const count = this.#items.length;
          this.#drafts = drafts ?? previousDrafts;
          if (page.batches.length) {
            for (const batch of page.batches) this.#items.push(...batch.items);
            this.#tail = page.next ?? null;
            if (this.controls.live) this.#checkpoints = [...previousCheckpoints, ...page.batches.map(batch => batch.receipt)].slice(-96);
          }
          try {
            update();
          } catch (error) {
            this.#items.length = count;
            this.#tail = previous;
            this.#drafts = previousDrafts;
            this.#checkpoints = previousCheckpoints;
            throw error;
          }
          if (this.#tail !== previous) {
            void inspectExecution();
            if (this.#refresh && this.#tail && this.#tail.sequence > this.#refresh.expected.sequence) void this.#checkRefresh();
          }
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

    const resumeSaved = () => {
      if (!resumeKeep || this.#keeping) return;
      resumeKeep = false;
      observeKeep();
    };
    this.#changes.addEventListener("change", () => { update(); void showSelectedModel(); void observe(); void inspectExecution(); resumeSaved(); }, { signal });
    signal.addEventListener("abort", () => { void unmount(transcript); }, { once: true });
    draw();
    update();
    void showSelectedModel();
    void observe();
    void inspectExecution();
    if (this.#disposal) void this.#checkDiscard(signal);
    else if (this.#pending && !this.#busy) void this.#check();
    else if (this.#refresh) void this.#checkRefresh();
    resumeSaved();
  }
}
