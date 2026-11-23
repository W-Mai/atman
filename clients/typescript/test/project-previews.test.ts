import { expect, test } from "bun:test";
import type { Dtos } from "../src/index.ts";
import { ProjectPreviews } from "../web/project-previews.ts";

type Snapshot = Dtos["FlowPreviewSnapshot"];
type Response = Dtos["ReadFlowPreviewResponse"];

function snapshot(flow: Dtos["FlowAddress"], revision: number, outcome: Dtos["FlowPreviewOutcome"]): Snapshot {
  return { flow, edition: { host_generation: "host", worker_incarnation: flow.instance_id, revision },
    head: { branch: "branch", version: 1 }, text: `${flow.instance_id}:${revision}`, active: null,
    completion: { lease: { flow, turn: `${revision}`, run: `${revision}`, epoch: revision }, outcome } };
}

function target(session_id: string, instance_id: string) {
  const address = { session_id, instance_id };
  const calls: { known?: Dtos["FlowPreviewEdition"]; signal: AbortSignal; respond: (result: Response) => void }[] = [];
  const flow = { address, preview: (request: { known?: Dtos["FlowPreviewEdition"] | null } = {}, options: { signal?: AbortSignal } = {}) => {
    const { promise, resolve } = Promise.withResolvers<Response>();
    const signal = options.signal!;
    const respond = (result: Response) => resolve(result);
    calls.push({ ...(request.known == null ? {} : { known: request.known }), signal, respond });
    return promise.then((result) => ({ flow: address, result }));
  } };
  return { address, calls, flow };
}

async function until(predicate: () => boolean): Promise<void> {
  const deadline = performance.now() + 1_500;
  while (!predicate()) {
    if (performance.now() >= deadline) throw new Error("Preview did not reach the expected state");
    await Bun.sleep(5);
  }
}

test("retired Flow replies cannot mark a replacement unread; only a new outcome in its edition can", async () => {
  const a = target("session", "alpha");
  const b = target("session", "beta");
  const previews = new ProjectPreviews(() => {}, () => new Set());
  try {
    previews.focus(a.flow);
    a.calls[0]!.respond({ state: "ready", snapshot: snapshot(a.address, 1, "completed") });
    await until(() => a.calls.length === 2);
    expect(a.calls[1]?.known?.revision).toBe(1);
    previews.focus(b.flow);
    expect(a.calls[1]?.signal.aborted).toBe(true);
    b.calls[0]!.respond({ state: "ready", snapshot: snapshot(b.address, 1, "completed") });
    a.calls[1]!.respond({ state: "ready", snapshot: snapshot(a.address, 2, "failed") });
    await until(() => b.calls.length === 2);
    expect(previews.view("session")).toEqual({ text: "beta:1", working: false, unread: undefined });
    b.calls[1]!.respond({ state: "ready", snapshot: snapshot(b.address, 2, "cancelled") });
    await until(() => previews.view("session").unread === "cancelled");
    previews.focus(b.flow);
    expect(previews.view("session").unread).toBeUndefined();
  } finally {
    previews.close();
    for (const binding of [a, b]) for (const call of binding.calls) call.respond({ state: "unavailable", flow: binding.address });
  }
});

test("a newly focused row reclaims a wait slot and overflow rows get paced snapshots", async () => {
  const flows = Array.from({ length: 10 }, (_, index) => target(`${index}`, `${index}`));
  const previews = new ProjectPreviews(() => {}, () => new Set());
  try {
    for (const binding of flows) previews.keep(binding.flow);
    const immediate = flows.filter(({ calls }) => calls.length > 0);
    expect(immediate.length).toBeGreaterThan(6);
    expect(immediate.length).toBeLessThanOrEqual(8);
    const overflow = immediate[6]!;
    const late = flows[9]!;
    previews.focus(late.flow);
    const preempted = flows.slice(0, 6).find(({ calls }) => calls[0]?.signal.aborted);
    expect(preempted).toBeDefined();
    expect(late.calls.length).toBe(0);
    preempted!.calls[0]!.respond({ state: "unavailable", flow: preempted!.address });
    await until(() => late.calls.length === 1);
    expect(late.calls[0]?.signal.aborted).toBe(false);
    overflow.calls[0]!.respond({ state: "unavailable", flow: overflow.address });
    await until(() => flows[7]!.calls.length > 0);
  } finally {
    previews.close();
    for (const binding of flows) for (const call of binding.calls) call.respond({ state: "unavailable", flow: binding.address });
  }
});
