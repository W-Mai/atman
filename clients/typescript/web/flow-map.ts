import type { Dtos } from "../src/index.ts";

export type FlowMapEntry = {
  address: Dtos["FlowAddress"];
  title: string;
  parent?: Dtos["FlowAddress"];
  phase?: Dtos["FlowExecutionPhase"];
  paused?: boolean;
  archived?: boolean;
  unavailable?: boolean;
};

export type FlowMapView = {
  entries: readonly FlowMapEntry[];
  session: string;
  selected?: Dtos["FlowAddress"];
  pending?: boolean;
  hasRelations: boolean;
  refresh: () => void;
  more?: () => Promise<void>;
  onOpen: (address: Dtos["FlowAddress"]) => void;
};

export const executionPhase: Readonly<Record<string, string>> = {
  prepared: "Prepared", idle: "Idle", running: "Running", waiting: "Waiting", finishing: "Finishing",
  cancelling: "Stopping", detached_active: "Execution not attached", interrupted: "Interrupted",
  closing: "Closing", closed: "Closed",
};

export const mapKey = ({ session_id, instance_id }: Dtos["FlowAddress"]): string => `${session_id}/${instance_id}`;

type TreeRow = { key: string; entry: FlowMapEntry; depth: number; stems: readonly boolean[]; last: boolean; hasChildren: boolean };

export function flowMapRows(entries: readonly FlowMapEntry[], session: string): TreeRow[] {
  const nodes = new Map(entries.filter(entry => entry.address.session_id === session)
    .map(entry => [mapKey(entry.address), entry]));
  const parents = new Map<string, string>();
  for (const [key, entry] of nodes) {
    if (!entry.parent || entry.unavailable) continue;
    const parent = mapKey(entry.parent);
    if (parent !== key && nodes.has(parent) && !nodes.get(parent)!.unavailable) parents.set(key, parent);
  }

  // A malformed cycle has no truthful root or directed layout. Drop only its edges.
  const checked = new Set<string>();
  for (const key of parents.keys()) {
    const path = new Set<string>();
    let next: string | undefined = key;
    while (next !== undefined && parents.has(next) && !checked.has(next)) {
      if (path.has(next)) {
        let cyclic = false;
        for (const item of path) {
          cyclic ||= item === next;
          if (cyclic) parents.delete(item);
        }
        break;
      }
      path.add(next);
      next = parents.get(next);
    }
    for (const item of path) checked.add(item);
  }

  const roots: string[] = [];
  const children = new Map<string, string[]>();
  for (const key of nodes.keys()) {
    const parent = parents.get(key);
    if (parent === undefined) roots.push(key);
    else {
      const group = children.get(parent) ?? [];
      group.push(key);
      children.set(parent, group);
    }
  }
  const rows: TreeRow[] = [];
  const stack: Array<{key: string; depth: number; stems: readonly boolean[]; last: boolean}> = [];
  function enqueue(group: readonly string[], depth: number, stems: readonly boolean[]) {
    for (let index = group.length - 1; index >= 0; index--) {
      stack.push({ key: group[index]!, depth, stems, last: index === group.length - 1 });
    }
  }
  enqueue(roots, 0, []);
  while (stack.length) {
    const row = stack.pop()!;
    const group = children.get(row.key) ?? [];
    rows.push({ ...row, entry: nodes.get(row.key)!, hasChildren: group.length > 0 });
    if (group.length) {
      // Keep only the columns that are actually drawn; large chains remain linear.
      const stems = row.depth ? [...row.stems.slice(-11), !row.last] : row.stems;
      enqueue(group, row.depth + 1, stems);
    }
  }
  return rows;
}
