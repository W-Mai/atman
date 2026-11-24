import { expect, test } from "bun:test";
import { contract, cursorId, DeviceClient, methods } from "../src/index.ts";
import type { DeviceCredentials, DeviceRegistration, DurableCursor } from "../src/index.ts";

const origin = "https://device.example:65100";
const id = "11111111-1111-1111-1111-111111111111";
const session = "22222222-2222-2222-2222-222222222222";
const instance = "33333333-3333-3333-3333-333333333333";
const rotation = "44444444-4444-4444-4444-444444444444";
const device = {
  id, label: "Phone", scope: { session_id: session, permission: "read" },
  expires_at: "2026-12-20T08:00:00Z", status: "active",
} satisfies DeviceRegistration;
const first = { origin, device, access_token: "old-access", refresh_token: "old-refresh",
  access_expires_at: "2026-11-24T08:00:00Z" } satisfies DeviceCredentials;

test("device exchanges stay on the confirmed origin and use one original rotation identity", async () => {
  let requests = 0;
  const rotationBodies: string[] = [];
  let wrongDevice = false;
  const devices = new DeviceClient({ origin, fetch: async (url, init) => {
    requests += 1;
    const target = new URL(String(url));
    expect(target.origin).toBe(origin);
    expect(target.search + target.hash).toBe("");
    expect(init?.redirect).toBe("error");
    expect(init?.credentials).toBe("omit");
    expect(init?.referrerPolicy).toBe("no-referrer");
    const authorization = new Headers(init?.headers).get("Authorization");
    switch (target.pathname) {
      case "/devices/pair":
        expect(authorization).toBeNull();
        expect(init?.body).toBe('{"code":"fixture-code"}');
        return Response.json(first);
      case "/devices/refresh":
        expect(authorization).toBe("Bearer old-refresh");
        rotationBodies.push(String(init?.body));
        if (rotationBodies.length === 1) throw new Error("lost response");
        return Response.json({ ...first, device: wrongDevice ? { ...device, id: instance } : device,
          access_token: "new-access", refresh_token: "new-refresh" });
      case "/devices/me":
        expect(init?.method).toBe("GET");
        expect(authorization).toBe("Bearer new-access");
        return Response.json(device);
      default: throw new Error("unexpected route");
    }
  } });

  await expect(devices.pair("https://other.example/#pair=fixture-code")).rejects.toMatchObject({ kind: "source", outcome: "not_sent" });
  await expect(devices.pair(`${origin}/#pair=one&pair=two`)).rejects.toMatchObject({ kind: "request", outcome: "not_sent" });
  expect(() => devices.connect({ ...first, origin: "https://other.example" })).toThrow();
  expect(requests).toBe(0);
  const paired = await devices.pair(`${origin}/#pair=fixture-code`);
  expect(paired).toEqual(first);
  await expect(devices.refresh(paired, rotation)).rejects.toMatchObject({ kind: "transport", outcome: "unknown" });
  const refreshed = await devices.refresh(paired, rotation);
  expect(rotationBodies).toEqual([JSON.stringify({ request_id: rotation }), JSON.stringify({ request_id: rotation })]);
  expect(await devices.connect(refreshed).me()).toEqual(device);
  expect(JSON.stringify(devices.connect(refreshed))).not.toContain("new-access");
  wrongDevice = true;
  await expect(devices.refresh(paired, rotation)).rejects.toMatchObject({ kind: "source", outcome: "unknown" });
});

test("a paired Flow resumes the original journal and rejects private event content", async () => {
  const after: DurableCursor = { journal_id: instance, sequence: 7, digest: "1".repeat(64) };
  const receipt = { journal_id: instance, sequence: 8, digest: "2".repeat(64) } satisfies DurableCursor;
  const bad = { receipt, facts: [{ internal_record: "fixture" }] };
  const good = { receipt, facts: [] };
  let privateEvents = true;
  const devices = new DeviceClient({ origin, fetch: async (url, init) => {
    const target = new URL(String(url));
    expect(new Headers(init?.headers).get("Authorization")).toBe("Bearer old-access");
    if (target.pathname === "/events") {
      expect(target.searchParams.get("session_id")).toBe(session);
      expect(target.searchParams.get("instance_id")).toBe(instance);
      expect(target.searchParams.get("after")).toBe(cursorId(after));
      const data = JSON.stringify(privateEvents ? bad : good);
      return new Response(`event: flow_update\nid: ${cursorId(receipt)}\ndata: ${data}\n\n`, {
        headers: { "content-type": "text/event-stream" },
      });
    }
    expect(target.pathname).toBe("/rpc");
    const request = JSON.parse(String(init?.body));
    let result: unknown;
    if (request.method === "capabilities") {
      const { revision, request_schema, response_schema } = methods["flow.events"];
      result = { ...contract, host_generation: "paired", methods: { "flow.events": { revision, request_schema, response_schema } } };
    } else {
      expect(request.method).toBe("flow.events");
      expect(request.params.flow).toEqual({ session_id: session, instance_id: instance });
      expect(request.params.after).toEqual(after);
      result = { batches: [privateEvents ? bad : good], next: receipt, has_more: false };
    }
    return Response.json({ jsonrpc: "2.0", id: request.id, result });
  } });
  const flow = devices.connect(first).flow(instance);
  await expect(flow.events({ after })).rejects.toMatchObject({ kind: "source" });
  const wrong = await flow.observeEvents({ after });
  await expect(wrong.next()).rejects.toMatchObject({ kind: "protocol" });
  expect(wrong.last).toEqual(after);

  privateEvents = false;
  expect((await flow.events({ after })).result.batches).toEqual([good]);
  const resumed = await flow.observeEvents({ after });
  expect(await resumed.next()).toEqual({ value: good, done: false });
  expect(resumed.last).toEqual(receipt);
  await resumed.close();
});
