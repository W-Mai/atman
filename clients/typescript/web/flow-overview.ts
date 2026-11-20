import type { Dtos, FlowClient } from "../src/index.ts";

export type OverviewView = {
  key: string;
  snapshot: Dtos["FlowOverviewResponse"] | undefined;
  reading: boolean;
  failed: boolean;
};

type Target = { flow: FlowClient; label: string };

export class FlowOverview {
  #target: Target | undefined;
  #view: OverviewView | undefined;
  #visible = false;
  #read: AbortController | undefined;
  #timer: ReturnType<typeof setTimeout> | undefined;
  #dirty = false;
  readonly #present: (view: OverviewView | undefined) => void;

  constructor(present: (view: OverviewView | undefined) => void) {
    this.#present = present;
  }

  select(target: Target | undefined): void {
    if (target === this.#target) return;
    this.#cancel();
    this.#target = target;
    this.#install(target && { key: `${target.flow.address.session_id}/${target.flow.address.instance_id}`,
      snapshot: undefined, reading: true, failed: false });
    this.refresh();
  }

  visible(value: boolean): void {
    if (this.#visible === value) return;
    this.#visible = value;
    if (value) this.refresh();
    else this.#cancel();
  }

  invalidate(flow: FlowClient): void {
    if (this.#target?.flow === flow) this.refresh();
  }

  refresh(): void {
    if (!this.#visible || !this.#target) return;
    this.#dirty = true;
    if (this.#read || this.#timer !== undefined) return;
    this.#timer = setTimeout(() => {
      this.#timer = undefined;
      void this.#fetch();
    }, 100);
  }

  #install(view: OverviewView | undefined): void {
    this.#view = view;
    this.#present(view);
  }

  #cancel(): void {
    clearTimeout(this.#timer);
    this.#timer = undefined;
    this.#read?.abort();
    this.#read = undefined;
    this.#dirty = false;
  }

  async #fetch(): Promise<void> {
    const target = this.#target;
    if (!target || !this.#visible) return;
    const read = new AbortController();
    this.#read = read;
    this.#dirty = false;
    const view = this.#view!;
    const previous = view.snapshot;
    this.#install({ ...view, reading: true, failed: false });
    try {
      const { result } = await target.flow.overview({ signal: read.signal });
      if (this.#read !== read) return;
      if (previous && previous.cursor.journal_id === result.cursor.journal_id &&
          previous.cursor.sequence > result.cursor.sequence) {
        this.#install({ ...view, reading: false, failed: true });
        return;
      }
      this.#install({ ...view, snapshot: result, reading: false, failed: false });
    } catch {
      if (this.#read !== read) return;
      this.#install({ ...view, reading: false, failed: true });
    } finally {
      if (this.#read === read) {
        this.#read = undefined;
        if (this.#dirty) this.refresh();
      }
    }
  }
}
