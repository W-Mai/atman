import type { Dtos } from "../src/index.ts";
import { leaseKey } from "./live-projection.ts";

export type DiffTarget = Readonly<{
  key: string;
  title: string;
  locator: Dtos["ConversationDiffLocator"];
}>;
export type DiffTargets = Map<string, DiffTarget[]>;
export type ReadDiff = (target: DiffTarget, offset: number, limit: number, signal: AbortSignal) =>
  Promise<Dtos["ReadConversationDiffResponse"]>;

export function recordWorkDiffs(targets: DiffTargets, page: Dtos["ConversationPage"]): void {
  for (const item of page.items) if (item.work) targets.delete(leaseKey(item.work.lease));
  const { source, head } = page;
  for (const { lease, title, record, receipt } of page.work_diffs?.entries ?? []) {
    const group = leaseKey(lease);
    const patches = targets.get(group) ?? [];
    patches.push({
      key: JSON.stringify([group, head.branch, head.version, receipt.digest, record.fact, record.item]),
      title,
      locator: { source, head, receipt, record },
    });
    targets.set(group, patches);
  }
  // Keep indices for displayed/paged Work while bounding retained view metadata.
  while (targets.size > 1024) targets.delete(targets.keys().next().value!);
}
