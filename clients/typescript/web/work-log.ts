import type { Dtos } from "../src/index.ts";
import { leaseKey } from "./live-projection.ts";

type Reply = Dtos["ReadConversationLogResponse"];
export const LOG_PAGE_BYTES = 8192;
export type LogProgress = Extract<Reply, { state: "verifying" }>;
export type LogTarget = Readonly<{
  key: string;
  locator: Dtos["ConversationLogLocator"];
  bytes: number | undefined;
  drained: boolean | undefined;
  epoch: number;
}>;
export type LogTargets = Map<string, LogTarget[]>;
export type ReadLog = (target: LogTarget, offset: number, signal: AbortSignal, progress: (value: LogProgress) => void) =>
  Promise<Exclude<Reply, LogProgress>>;
export type LogObservation = Readonly<{ targets: readonly LogTarget[]; read: ReadLog }>;

function compareRecord(a: LogTarget["locator"]["record"], b: LogTarget["locator"]["record"]): number {
  return a.commit - b.commit || a.fact - b.fact || a.item - b.item;
}

export function sameCheckpoint(a: LogTarget, b: LogTarget): boolean {
  return a.key === b.key && a.locator.receipt.digest === b.locator.receipt.digest &&
    compareRecord(a.locator.record, b.locator.record) === 0;
}

export function recordWorkLogs(targets: LogTargets, page: Dtos["ConversationPage"]): void {
  const { source, head } = page;
  for (const { lease, handle, bytes, drained, receipt, record } of page.work_logs?.entries ?? []) {
    const group = leaseKey(lease);
    const key = JSON.stringify([group, head.branch, head.version, handle]);
    const previous = targets.get(group) ?? [];
    const index = previous.findIndex(log => log.key === key);
    const target = { key, locator: { source, head, receipt, record }, bytes: bytes ?? undefined,
      drained: drained ?? undefined, epoch: lease.epoch };
    const current = previous[index];
    if (current) {
      const recorded = Number(target.bytes != null) - Number(current.bytes != null);
      if (recorded < 0 || (recorded === 0 && compareRecord(current.locator.record, record) >= 0)) continue;
    }
    targets.set(group, index === -1 ? [...previous.slice(-63), target] :
      previous.map((log, at) => index === at ? target : log));
  }
  while (targets.size > 128) targets.delete(targets.keys().next().value!);
}

export function activityLogs(targets: LogTargets): readonly LogTarget[] {
  return [...targets.values()].flat()
    .sort((a, b) => b.epoch - a.epoch)
    .slice(0, 6);
}
