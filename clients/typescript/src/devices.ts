import { HttpClient, requestJson, transportSettings } from "./client.ts";
import type { Fetch, HttpTransportOptions, RequestOptions } from "./client.ts";
import { canonicalUuid } from "./cursor.ts";
import { ClientError } from "./errors.ts";
import type { CommandOutcome } from "./errors.ts";
import { FlowClient, SessionClient } from "./facade.ts";
import { contract } from "./generated.ts";
import type { Dtos } from "./generated.ts";

const deviceIdHeader = "x-atman-device-id";

export interface DeviceClientOptions extends HttpTransportOptions {
  /** HTTPS origin confirmed by the person pairing the device. */
  readonly origin: string;
}

export interface DeviceRegistration {
  readonly id: string;
  readonly label: string;
  readonly scope: { readonly session_id: string; readonly permission: "read" | "send" | "approve" | "send_and_approve" };
  readonly expires_at: string;
  readonly status: "active";
}

/** An origin-bound credential pair for platform secure storage. */
export interface DeviceCredentials {
  readonly origin: string;
  readonly device: DeviceRegistration;
  readonly access_token: string;
  readonly access_expires_at: string;
  readonly refresh_token: string;
}

export interface BrowserDeviceSession {
  readonly device: DeviceRegistration;
  readonly access_expires_at: string;
}

export interface SharedSession {
  readonly session_id: string;
  readonly title: string | null;
  readonly project: string | null;
  readonly default_flow: string | null;
}

function httpsUrl(value: string): URL {
  let url: URL;
  try { url = new URL(value); }
  catch { throw new ClientError("request", "Expected an absolute HTTPS URL", "not_sent"); }
  if (url.protocol !== "https:" || url.username || url.password || url.pathname !== "/" || url.search) {
    throw new ClientError("request", "Expected an HTTPS origin without credentials, path or query", "not_sent");
  }
  return url;
}

function invitationCode(origin: string, invitation: string): string {
  const url = httpsUrl(invitation);
  if (url.origin !== origin) throw new ClientError("source", "Invitation belongs to another HTTPS origin", "not_sent");
  const entries = Array.from(new URLSearchParams(url.hash.slice(1)));
  const pair = entries[0];
  if (entries.length !== 1 || pair?.[0] !== "pair" || !/^[A-Za-z0-9._-]{1,256}$/.test(pair[1])) {
    throw new ClientError("request", "Invitation must contain one pairing code", "not_sent");
  }
  return pair[1];
}

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("Expected a device record");
  return value as Record<string, unknown>;
}

function uuid(value: unknown): string {
  if (typeof value !== "string") throw new Error("Expected a device UUID");
  return canonicalUuid(value);
}

function timestamp(value: unknown): string {
  if (typeof value !== "string" || !Number.isFinite(Date.parse(value))) throw new Error("Expected a device expiration timestamp");
  return value;
}

function token(value: unknown): string {
  if (typeof value !== "string" || !/^[\x21-\x7e]{1,1024}$/.test(value)) throw new Error("Expected a device bearer token");
  return value;
}

function registration(value: unknown): DeviceRegistration {
  const raw = object(value);
  const scope = object(raw.scope);
  if (typeof raw.label !== "string" || raw.label.length === 0 ||
      (scope.permission !== "read" && scope.permission !== "send" &&
        scope.permission !== "approve" && scope.permission !== "send_and_approve") || raw.status !== "active") {
    throw new Error("Expected an active device registration");
  }
  return Object.freeze({ id: uuid(raw.id), label: raw.label, expires_at: timestamp(raw.expires_at), status: raw.status,
    scope: Object.freeze({ session_id: uuid(scope.session_id), permission: scope.permission }) });
}

function credentials(origin: string, value: unknown): DeviceCredentials {
  const raw = object(value);
  const device = registration(raw.device);
  const access = token(raw.access_token);
  const refresh = token(raw.refresh_token);
  const accessExpiresAt = timestamp(raw.access_expires_at);
  if (access === refresh || Date.parse(accessExpiresAt) > Date.parse(device.expires_at)) {
    throw new Error("Expected independent credentials within the device lifetime");
  }
  return Object.freeze({ origin, device, access_token: access, access_expires_at: accessExpiresAt, refresh_token: refresh });
}

function sameDevice(expected: DeviceRegistration, actual: DeviceRegistration, outcome: CommandOutcome): void {
  if (expected.id !== actual.id || expected.scope.session_id !== actual.scope.session_id ||
      expected.scope.permission !== actual.scope.permission) {
    throw new ClientError("source", "Device reply belongs to another grant", outcome);
  }
}

function rotationId(value: string): string {
  try { return canonicalUuid(value); }
  catch { throw new ClientError("request", "Expected a non-nil rotation request ID", "not_sent"); }
}

/** Device APIs bind credential exchange and reads to one verified origin. */
export class DeviceClient {
  readonly origin: string;
  readonly #transport: ReturnType<typeof transportSettings>;

  constructor(options: DeviceClientOptions) {
    const url = httpsUrl(options.origin);
    if (url.hash) throw new ClientError("request", "Device origin must not contain a fragment", "not_sent");
    this.origin = url.origin;
    this.#transport = transportSettings(options);
    Object.freeze(this);
  }

  async pair(invitation: string, options: RequestOptions = {}): Promise<DeviceCredentials> {
    const code = invitationCode(this.origin, invitation);
    const reply = await this.#deviceRequest("pair", { code }, undefined, options, "unknown");
    return this.#issued(reply);
  }

  /** Retry an uncertain rotation with the original credential pair and request ID. */
  async refresh(current: DeviceCredentials, requestId: string, options: RequestOptions = {}): Promise<DeviceCredentials> {
    const original = this.#capture(current);
    const request_id = rotationId(requestId);
    const reply = this.#issued(await this.#deviceRequest("refresh", { request_id }, original.refresh_token, options, "unknown"));
    sameDevice(original.device, reply.device, "unknown");
    if (reply.access_token === original.access_token || reply.refresh_token === original.refresh_token) {
      throw new ClientError("protocol", "Device rotation did not issue a new credential pair", "unknown");
    }
    return reply;
  }

  connect(current: DeviceCredentials): PairedDeviceClient {
    const granted = this.#capture(current);
    const { fetch, timeout, maxBytes } = this.#transport;
    const http = new HttpClient({ endpoint: this.origin, token: granted.access_token, contract, fetch,
      requestTimeoutMs: timeout, maxResponseBytes: maxBytes });
    return new PairedDeviceClient(http, granted.device, async (options = {}) => {
      const reply = await this.#deviceRequest("me", undefined, granted.access_token, options, "not_applicable");
      let device: DeviceRegistration;
      try { device = registration(reply); }
      catch { throw new ClientError("protocol", "Device registration is invalid"); }
      sameDevice(granted.device, device, "not_applicable");
      return device;
    });
  }

  #capture(value: DeviceCredentials): DeviceCredentials {
    if (value?.origin !== this.origin) {
      throw new ClientError("source", "Credentials belong to another HTTPS origin", "not_sent");
    }
    try { return credentials(this.origin, value); }
    catch { throw new ClientError("request", "Device credential pair is invalid", "not_sent"); }
  }

  #issued(value: unknown): DeviceCredentials {
    try { return credentials(this.origin, value); }
    catch { throw new ClientError("protocol", "Device credential response is invalid", "unknown"); }
  }

  #deviceRequest(route: "pair" | "refresh" | "me", body: object | undefined, access: string | undefined,
    options: RequestOptions, outcome: CommandOutcome): Promise<unknown> {
    const headers = new Headers({ accept: "application/json" });
    if (access !== undefined) headers.set("authorization", `Bearer ${access}`);
    const init = body === undefined ? { method: "GET", headers } : { method: "POST", headers, body: JSON.stringify(body) };
    if (body !== undefined) headers.set("content-type", "application/json");
    const transport = { ...this.#transport, maxBytes: Math.min(8192, this.#transport.maxBytes) };
    return requestJson(transport, `${this.origin}/devices/${route}`, init, options, outcome);
  }
}

/** Device access on the current HTTPS origin using the browser's HttpOnly credential. */
export class BrowserDeviceClient {
  readonly origin: string;
  readonly #transport: ReturnType<typeof transportSettings>;

  constructor(options: DeviceClientOptions) {
    const url = httpsUrl(options.origin);
    if (url.hash || (globalThis.location !== undefined && url.origin !== globalThis.location.origin)) {
      throw new ClientError("request", "Browser credentials require the current HTTPS origin", "not_sent");
    }
    this.origin = url.origin;
    this.#transport = transportSettings(options, "same-origin");
  }

  pair(invitation: string, options: RequestOptions = {}): Promise<BrowserDeviceSession> {
    return this.#authenticate("pair", { code: invitationCode(this.origin, invitation) }, options);
  }

  refresh(requestId: string, options: RequestOptions = {}): Promise<BrowserDeviceSession> {
    return this.#authenticate("refresh", { request_id: rotationId(requestId) }, options);
  }

  me(options: RequestOptions = {}): Promise<BrowserDeviceSession> {
    return this.#authenticate("me", undefined, options);
  }

  async signOut(options: RequestOptions = {}): Promise<void> {
    await this.#request("sign-out", {}, options, "unknown");
  }

  async session(grant: BrowserDeviceSession, options: RequestOptions = {}): Promise<SharedSession> {
    const value = await this.#request("session", undefined, options, "not_applicable", grant.device.id);
    try {
      const raw = object(value);
      const session_id = uuid(raw.session_id);
      const { title, project, default_flow } = raw;
      if ((title !== null && typeof title !== "string") || (project !== null && typeof project !== "string")) throw new Error("Invalid shared session labels");
      if (session_id !== grant.device.scope.session_id) throw new ClientError("source", "Shared Session differs from the browser grant");
      return { session_id, title, project, default_flow: default_flow === null ? null : uuid(default_flow) };
    } catch (error) {
      if (error instanceof ClientError && error.kind === "source") throw error;
      throw new ClientError("protocol", "Shared Session response is invalid");
    }
  }

  connect(grant: BrowserDeviceSession, send?: Fetch): PairedDeviceClient {
    const device = grant.device;
    const { fetch, timeout, maxBytes } = this.#transport;
    const dispatch = send ?? fetch;
    const http = new HttpClient({ endpoint: this.origin, cookie: "same-origin", contract, fetch: (input, init) => {
      const headers = new Headers(init?.headers);
      headers.set(deviceIdHeader, device.id);
      return dispatch(input, { ...init, headers });
    },
      requestTimeoutMs: timeout, maxResponseBytes: maxBytes });
    return new PairedDeviceClient(http, device, async (options) => {
      const current = await this.me(options);
      sameDevice(device, current.device, "not_applicable");
      return current.device;
    });
  }

  async #authenticate(route: "pair" | "refresh" | "me", body: object | undefined, options: RequestOptions): Promise<BrowserDeviceSession> {
    const outcome = body === undefined ? "not_applicable" : "unknown";
    const response = await this.#request(route, body, options, outcome);
    try {
      const data = object(response);
      const device = registration(data.device);
      const access_expires_at = timestamp(data.access_expires_at);
      if (Date.parse(access_expires_at) > Date.parse(device.expires_at)) throw new Error("Access outlives the grant");
      return Object.freeze({ device, access_expires_at });
    } catch {
      throw new ClientError("protocol", "Browser device response is invalid", outcome);
    }
  }

  #request(route: string, body: object | undefined, options: RequestOptions, outcome: CommandOutcome, deviceId?: string): Promise<unknown> {
    const headers = new Headers({ accept: "application/json" });
    if (deviceId !== undefined) headers.set(deviceIdHeader, deviceId);
    const init = body === undefined ? { method: "GET", headers } : { method: "POST", headers, body: JSON.stringify(body) };
    if (body !== undefined) headers.set("content-type", "application/json");
    const transport = { ...this.#transport, maxBytes: Math.min(8192, this.#transport.maxBytes) };
    return requestJson(transport, `${this.origin}/devices/browser/${route}`, init, options, outcome);
  }
}

/** Authenticated access to the Session recorded in the device grant. */
class PairedDeviceClient {
  readonly #http: HttpClient;
  readonly #session: SessionClient;

  constructor(http: HttpClient, readonly device: DeviceRegistration,
    readonly me: (options?: RequestOptions) => Promise<DeviceRegistration>) {
    this.#http = http;
    this.#session = new SessionClient(http, device.scope.session_id);
    Object.freeze(this);
  }

  capabilities(options: RequestOptions = {}) { return this.#http.capabilities(options); }

  flows(request: Pick<Dtos["ListFlowsRequest"], "before" | "limit"> = {}, options: RequestOptions = {}) {
    return this.#session.flows(request, options);
  }

  flow(instanceId: string): FlowClient {
    return new FlowClient(this.#http, { session_id: this.device.scope.session_id, instance_id: instanceId }, "receipts");
  }
}

export type { PairedDeviceClient };
