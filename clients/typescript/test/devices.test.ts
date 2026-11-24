import { expect, test } from "bun:test";
import { BrowserDeviceClient, contract, cursorId, DeviceClient, methods, NativeDeviceSession } from "../src/index.ts";
import type { DeviceCredentials, DeviceNotification, DeviceRegistration, DurableCursor, StoredDevice } from "../src/index.ts";

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

test("notification lookup binds the opaque ID to the original grant on bearer and cookie transports", async () => {
  const flow = { session_id: session, instance_id: instance };
  let response: DeviceNotification = { flow, kind: "turn_completed" };
  let status = 200;
  const seen: string[] = [];
  const fetch = async (input: URL | RequestInfo, init?: RequestInit) => {
    const url = new URL(String(input));
    expect(url.href).toBe(`${origin}/devices/notifications/${rotation}`);
    expect(init?.method).toBe("GET");
    expect(init?.redirect).toBe("error");
    const headers = new Headers(init?.headers);
    if (init?.credentials === "same-origin") {
      expect(headers.get("x-atman-device-id")).toBe(id);
      expect(headers.get("authorization")).toBeNull();
    } else {
      expect(init?.credentials).toBe("omit");
      expect(headers.get("authorization")).toBe("Bearer old-access");
    }
    seen.push(init?.credentials ?? "");
    return Response.json(response, { status });
  };
  const native = new DeviceClient({ origin, fetch }).connect(first);
  const cookie = new BrowserDeviceClient({ origin }).connect(first, fetch);
  await expect(cookie.notification("../session")).rejects.toMatchObject({ kind: "request", outcome: "not_sent" });
  expect(seen).toEqual([]);
  expect(await native.notification(rotation)).toEqual(response);
  expect(await cookie.notification(rotation)).toEqual(response);
  expect(seen).toEqual(["omit", "same-origin"]);
  response = { flow: { ...flow, session_id: id }, kind: "turn_completed" };
  await expect(cookie.notification(rotation)).rejects.toMatchObject({ kind: "source" });
  response = { flow, kind: "approval_requested" };
  await expect(native.notification(rotation)).rejects.toMatchObject({ kind: "source" });
  const approve = new DeviceClient({ origin, fetch }).connect({ ...first, device: { ...device,
    scope: { ...device.scope, permission: "approve" } } });
  expect(await approve.notification(rotation)).toEqual(response);
  status = 404;
  await expect(cookie.notification(rotation)).rejects.toMatchObject({ status: 404 });
});

test("native renewal is checkpointed before dispatch and installs one whole pair before further requests", async () => {
  const expires_at = new Date(Date.now() + 86_400_000).toISOString();
  const initial = { ...first, device: { ...device, expires_at }, access_expires_at: new Date().toISOString() };
  const replacement = { ...initial, access_expires_at: new Date(Date.now() + 900_000).toISOString(),
    access_token: "new-access", refresh_token: "new-refresh" };
  let stored: StoredDevice | undefined = { credentials: initial };
  let failCheckpoint = true;
  let loseInstallation = true;
  const attempts: string[] = [];
  const store = {
    async load() { return stored; },
    async replace(value: StoredDevice | undefined) {
      if ((value?.renewal && failCheckpoint) || (value?.credentials.access_token === "new-access" && loseInstallation)) {
        throw new Error("secure storage unavailable");
      }
      stored = value;
    },
  };
  function start() {
    return new NativeDeviceSession(store, { fetch: async (input, init) => {
      const url = new URL(String(input));
      expect(url.origin).toBe(origin);
      const headers = new Headers(init?.headers);
      if (url.pathname === "/devices/refresh") {
        const id = JSON.parse(String(init?.body)).request_id;
        attempts.push(id);
        expect(stored).toEqual({ credentials: initial, renewal: id });
        expect(headers.get("authorization")).toBe("Bearer old-refresh");
        return Response.json(replacement);
      }
      expect(stored).toEqual({ credentials: replacement });
      expect(headers.get("authorization")).toBe("Bearer new-access");
      switch (url.pathname) {
        case "/devices/me": return Response.json(replacement.device);
        case "/devices/session": return Response.json({ session_id: session, title: null, project: null, default_flow: null });
        default: throw new Error("unexpected route");
      }
    } });
  }
  await expect(start().resume()).rejects.toThrow("secure storage unavailable");
  expect(attempts).toEqual([]);
  failCheckpoint = false;
  await expect(start().resume()).rejects.toThrow("secure storage unavailable");
  expect(attempts).toHaveLength(1);
  const request = attempts[0]!;
  expect(stored?.renewal).toBe(request);
  loseInstallation = false;
  const restored = start();
  const connected = await restored.resume();
  expect(attempts).toEqual([request, request]);
  expect(await restored.resume()).toBe(connected);
  expect((await connected?.session())?.session_id).toBe(session);
  await restored.signOut();
  expect(stored).toBeUndefined();
  await expect(connected?.session()).rejects.toMatchObject({ kind: "aborted" });
});

test("device exchanges stay on the confirmed origin and use one original rotation identity", async () => {
  let requests = 0;
  const rotationBodies: string[] = [];
  let wrongDevice = false;
  let otherSession = false;
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
      case "/devices/session":
        expect(init?.method).toBe("GET");
        expect(authorization).toBe("Bearer new-access");
        return Response.json({ session_id: otherSession ? id : session, title: "Shared workspace", project: null,
          default_flow: instance, ignored: "Host-only value" });
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
  const connection = devices.connect(refreshed);
  expect(await connection.me()).toEqual(device);
  expect(await connection.session()).toEqual({ session_id: session, title: "Shared workspace", project: null, default_flow: instance });
  otherSession = true;
  await expect(connection.session()).rejects.toMatchObject({ kind: "source" });
  expect(JSON.stringify(connection)).not.toContain("new-access");
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

test("browser pairing and scoped RPC share the same-origin cookie transport", async () => {
  const calls: string[] = [];
  const browser = new BrowserDeviceClient({ origin, fetch: async (input, init) => {
    const url = new URL(String(input));
    expect(url.origin).toBe(origin);
    const headers = new Headers(init?.headers);
    expect(headers.has("authorization")).toBe(false);
    expect(headers.get("x-atman-device-id")).toBe(url.pathname === "/devices/browser/pair" ? null : id);
    expect(init?.credentials).toBe("same-origin");
    expect(init?.mode).toBe("same-origin");
    expect(init?.redirect).toBe("error");
    expect(init?.referrerPolicy).toBe("same-origin");
    calls.push(url.pathname);
    if (url.pathname === "/devices/browser/pair") {
      return Response.json({ device, access_expires_at: first.access_expires_at });
    }
    if (url.pathname === "/devices/browser/session") {
      return Response.json({ session_id: instance, title: "Wrong Session", project: null, default_flow: null });
    }
    const request = JSON.parse(String(init?.body));
    return Response.json({ jsonrpc: "2.0", id: request.id, result: { ...contract, host_generation: "fixture", methods: {} } });
  } });
  expect(() => new BrowserDeviceClient({ origin: "http://device.example" })).toThrow();
  const installed = await browser.pair(`${origin}/#pair=fixture-code`);
  expect(Object.keys(installed).sort()).toEqual(["access_expires_at", "device"]);
  await expect(browser.session(installed)).rejects.toMatchObject({ kind: "source" });
  const connection = browser.connect(installed);
  await expect(connection.session()).rejects.toMatchObject({ kind: "source" });
  await connection.capabilities();
  expect(calls).toEqual(["/devices/browser/pair", "/devices/browser/session", "/devices/browser/session", "/rpc"]);
});
