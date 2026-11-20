import { canonicalUuid, decodeDto, ClientError, type Dtos, type FlowClient } from "../src/index.ts";

type Edit = Dtos["ComposerPreferenceEdit"];
type Response = Dtos["FlowComposerResponse"];
type Request = Omit<Dtos["SetComposerPreferenceRequest"], "flow">;

const archiveKey = "atman.web.composer-pending.v1";
const maximumRequests = 32;
type StoragePort = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export class PreferenceReceipts {
  #scope: string | undefined;
  #requests = new Map<string, Request>();

  constructor(private readonly storage?: StoragePort) {}

  connect(scope: string): void {
    if (this.#scope === scope) return;
    this.#scope = scope;
    this.#requests.clear();
    try {
      const raw = this.storage?.getItem(archiveKey);
      if (!raw || raw.length > 65536) return;
      const record = JSON.parse(raw);
      if (record.scope !== scope || !Array.isArray(record.requests)) return;
      for (const value of record.requests.slice(0, maximumRequests)) {
        try {
          const { flow, ...request } = decodeDto("SetComposerPreferenceRequest", value);
          canonicalUuid(request.request_id);
          if (/^[0-9a-f]{64}$/.test(request.expected_revision)) this.#requests.set(this.#key(flow), request);
        } catch { /* Ignore a malformed entry. */ }
      }
    } catch { /* Storage is optional. */ }
  }

  #key(flow: FlowClient["address"]): string {
    return `${canonicalUuid(flow.session_id)}/${canonicalUuid(flow.instance_id)}`;
  }

  read(scope: string, flow: FlowClient["address"]): Request | undefined {
    return this.#scope === scope ? this.#requests.get(this.#key(flow)) : undefined;
  }

  write(scope: string, flow: FlowClient["address"], request?: Request): boolean {
    if (this.#scope !== scope) return false;
    const key = this.#key(flow);
    if (request) {
      if (!this.#requests.has(key) && this.#requests.size >= maximumRequests) return false;
      this.#requests.set(key, request);
    } else this.#requests.delete(key);
    try {
      if (!this.#requests.size) this.storage?.removeItem(archiveKey);
      else this.storage?.setItem(archiveKey, JSON.stringify({ scope, requests: [...this.#requests].map(([key, value]) => {
        const [session_id, instance_id] = key.split("/");
        return { flow: { session_id, instance_id }, ...value };
      }) }));
    } catch { /* Keep the original intent in memory when storage is unavailable. */ }
    return true;
  }
}

export type PreferenceEditorView = Readonly<{
  busy: boolean;
  unsettled: boolean;
  retryable: boolean;
  message: string;
}>;

const rejection: Record<Dtos["ComposerPreferenceRejection"], string> = {
  invalid_selection: "This choice is no longer supported.",
  unavailable: "The selected provider is unavailable.",
  not_eager: "Escalation is available only in Eager mode.",
  invalid_target: "This Flow is no longer available.",
  different_intent: "This request matches a different change.",
  storage: "Could not save this setting.",
};

export class PreferenceEditor {
  #request: Request | undefined;
  #busy = false;
  #retryable = false;
  #message = "";

  constructor(private readonly flow: FlowClient, private readonly signal: AbortSignal,
    private readonly current: () => Response | undefined, private readonly changed: () => void,
    private readonly refresh: () => Promise<void>,
    private readonly receipt?: { archive: PreferenceReceipts; scope: string }) {
    this.#request = receipt?.archive.read(receipt.scope, flow.address);
    if (this.#request) this.#message = "Save is unconfirmed.";
  }

  #clear(): void {
    if (this.receipt) this.receipt.archive.write(this.receipt.scope, this.flow.address);
    this.#request = undefined;
  }

  view(): PreferenceEditorView {
    return { busy: this.#busy, unsettled: this.#request !== undefined, retryable: this.#retryable, message: this.#message };
  }

  edit(expected: string, edit: Edit): void {
    if (this.signal.aborted || this.#busy || this.#request ||
        this.current()?.edit_revisions?.[edit.field] !== expected) return;
    const request = { expected_revision: expected, request_id: crypto.randomUUID(), edit };
    if (this.receipt && !this.receipt.archive.write(this.receipt.scope, this.flow.address, request)) {
      this.#message = "Check earlier settings changes before saving another.";
      this.changed();
      return;
    }
    this.#request = request;
    void this.#dispatch(true);
  }

  check(): void { if (!this.#busy && this.#request) void this.#dispatch(false); }
  retry(): void { if (!this.#busy && this.#retryable && this.#request) void this.#dispatch(true, true); }

  async #dispatch(write: boolean, retry = false): Promise<void> {
    const request = this.#request;
    if (!request || this.signal.aborted) return;
    this.#busy = true;
    this.#retryable = false;
    this.#message = write ? "Saving…" : "Checking…";
    this.changed();
    try {
      const { result } = await (write
        ? this.flow.setComposerPreference(request, { signal: this.signal })
        : this.flow.composerPreferenceStatus(request.request_id, { signal: this.signal }));
      if (this.signal.aborted) return;
      const { outcome } = result;
      switch (outcome.status) {
        case "applied":
          this.#message = "Saved";
          this.#clear();
          break;
        case "conflict":
          this.#message = "This setting changed. Review the latest value.";
          this.#clear();
          break;
        case "rejected":
          this.#message = rejection[outcome.reason];
          this.#clear();
          break;
        case "not_found":
          this.#retryable = true;
          this.#message = "No receipt yet.";
          return;
        case "pending":
          this.#message = "Waiting for the owner to confirm.";
          return;
      }
      await this.refresh();
    } catch (error) {
      if (this.signal.aborted) return;
      if (write && !retry && error instanceof ClientError && error.outcome === "not_sent") {
        this.#clear();
        this.#message = "The change could not be sent.";
      } else if (this.#request) this.#message = "Save is unconfirmed.";
    } finally {
      this.#busy = false;
      if (!this.signal.aborted) this.changed();
    }
  }
}
