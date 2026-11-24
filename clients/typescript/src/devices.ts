import { HttpClient, requestJson, transportSettings } from "./client.ts";
import type { Fetch, HttpTransportOptions, RequestOptions } from "./client.ts";
import { canonicalUuid, flowSource } from "./cursor.ts";
import type { FlowSource } from "./cursor.ts";
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

export type DeviceNotification = Readonly<{
  flow: Readonly<FlowSource>;
  kind: "turn_completed" | "approval_requested";
}>;

export type WebPushSubscription = Readonly<{
  endpoint: string;
  p256dh: string;
  auth: string;
  application_server_key: string;
}>;

export type WebPushStatus = Readonly<{
  revision: string | null;
  enabled: boolean;
  application_server_key: string;
}>;

export type WebPushUpdate = PushUpdate<WebPushSubscription>;

export type NativePushPlatform = "apns" | "fcm";
export type NativePushStatus = PushStatus & Readonly<{ channel: string }>;
export type RelayPushStatus = PushStatus & Readonly<{ relay_origin: string }>;
export type NativePushUpdate = PushUpdate<Readonly<{
  platform: NativePushPlatform;
  channel: string;
  device_token: string;
}>>;
export type RelayPushUpdate = PushUpdate<Readonly<{ relay_origin: string; capability: string }>>;
export type RelayRegistrationUpdate = Readonly<{
  request_id: string;
  expected_revision: string | null;
  platform: NativePushPlatform;
  device_token: string | null;
}>;
export type RelayRegistration = Readonly<{
  revision: string;
  capability: string | null;
  expires_at_unix: number;
}>;

type PushStatus = Readonly<{ revision: string | null; enabled: boolean }>;
type PushUpdate<T> = Readonly<{ request_id: string; expected_revision: string | null; subscription: T | null }>;
type UpdateIdentity = Readonly<{ request_id: string; expected_revision: string | null }>;
type NativePushRoute = "notifications/subscription" | `notifications/native/${NativePushPlatform}`;
type NativePushRequest = (route: NativePushRoute, body: NativePushUpdate | RelayPushUpdate | undefined,
  options: RequestOptions) => Promise<unknown>;
type NotificationRoute = `notifications/${string}`;

function notificationDestination(value: unknown, device: DeviceRegistration): DeviceNotification {
  try {
    const { flow: address, kind } = object(value);
    const wire = object(address);
    const flow = flowSource({ session_id: uuid(wire.session_id), instance_id: uuid(wire.instance_id) });
    if (kind !== "turn_completed" && kind !== "approval_requested") throw new Error("Unknown notification kind");
    if (flow.session_id !== device.scope.session_id || (kind === "approval_requested" &&
        device.scope.permission !== "approve" && device.scope.permission !== "send_and_approve")) {
      throw new ClientError("source", "Notification target is outside the device grant");
    }
    return { flow, kind };
  } catch (error) {
    if (error instanceof ClientError && error.kind === "source") throw error;
    throw new ClientError("protocol", "Notification destination is invalid");
  }
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

function pairingCode(url: URL): string {
  const entries = Array.from(new URLSearchParams(url.hash.slice(1)));
  const pair = entries[0];
  if (entries.length !== 1 || pair?.[0] !== "pair" || !/^[A-Za-z0-9._-]{1,256}$/.test(pair[1])) {
    throw new ClientError("request", "Invitation must contain one pairing code", "not_sent");
  }
  return pair[1];
}

export function deviceInvitationOrigin(invitation: string): string {
  const url = httpsUrl(invitation);
  pairingCode(url);
  return url.origin;
}

function invitationCode(origin: string, invitation: string): string {
  const url = httpsUrl(invitation);
  if (url.origin !== origin) throw new ClientError("source", "Invitation belongs to another HTTPS origin", "not_sent");
  return pairingCode(url);
}

function object(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new Error("Expected a device record");
  return value as Record<string, unknown>;
}

function uuid(value: unknown): string {
  if (typeof value !== "string") throw new Error("Expected a device UUID");
  return canonicalUuid(value);
}

function pushStatus(raw: Record<string, unknown>, expected?: { revision: string | null; enabled: boolean }): PushStatus {
  const revision = raw.revision === null ? null : uuid(raw.revision);
  const { enabled } = raw;
  if (typeof enabled !== "boolean" || enabled && revision === null ||
      expected && (revision === null || revision === expected.revision || enabled !== expected.enabled)) {
    throw new Error("Push status changed identity");
  }
  return { revision, enabled };
}

function updateIdentity(request: UpdateIdentity): UpdateIdentity {
  return { request_id: uuid(request.request_id), expected_revision: request.expected_revision === null
    ? null : uuid(request.expected_revision) };
}

function checkPushPlatform(platform: NativePushPlatform): void {
  if (platform !== "apns" && platform !== "fcm") {
    throw new ClientError("request", "Unknown native push platform", "not_sent");
  }
}

function nativePushChannel(platform: NativePushPlatform, value: unknown): string {
  if (typeof value !== "string" || !value.startsWith(platform + ":") ||
      value.length <= platform.length + 1 || value.length > 272 || /[^A-Za-z0-9.:-]/.test(value)) {
    throw new Error("Invalid native push channel");
  }
  return value;
}

export function pushToken(platform: NativePushPlatform, value: unknown): string {
  checkPushPlatform(platform);
  if (typeof value !== "string" || (platform === "apns"
    ? value.length < 32 || value.length > 256 || value.length % 2 !== 0 || /[^a-f0-9]/.test(value)
    : value.length < 16 || value.length > 512 || /[^A-Za-z0-9:_.-]/.test(value))) {
    throw new ClientError("request", "Invalid native push token", "not_sent");
  }
  return value;
}

function relayOrigin(value: unknown): string {
  if (typeof value !== "string" || value.length > 320) throw new Error("Invalid relay origin");
  const url = httpsUrl(value);
  if (url.hash) throw new Error("Invalid relay origin");
  return url.origin;
}

function ownerKey(value: unknown): value is string {
  return typeof value === "string" && value.length === 43 && /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/.test(value);
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

function sharedSession(value: unknown, expected: string): SharedSession {
  try {
    const raw = object(value);
    const session_id = uuid(raw.session_id);
    const { title, project, default_flow } = raw;
    if ((title !== null && typeof title !== "string") || (project !== null && typeof project !== "string")) {
      throw new Error("Invalid shared session labels");
    }
    if (session_id !== expected) throw new ClientError("source", "Shared Session differs from the device grant");
    return { session_id, title, project, default_flow: default_flow === null ? null : uuid(default_flow) };
  } catch (error) {
    if (error instanceof ClientError && error.kind === "source") throw error;
    throw new ClientError("protocol", "Shared Session response is invalid");
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
    }, async (options = {}) => sharedSession(
      await this.#deviceRequest("session", undefined, granted.access_token, options, "not_applicable"),
      granted.device.scope.session_id,
    ), (route, options) => this.#deviceRequest(route, undefined, granted.access_token, options, "not_applicable"),
    { nativePush: new NativePushClient((route, body, options) =>
      this.#deviceRequest(route, body, granted.access_token, options, body === undefined ? "not_applicable" : "unknown")) });
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

  #deviceRequest(route: "pair" | "refresh" | "me" | "session" | NotificationRoute, body: object | undefined, access: string | undefined,
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

  async signOut(options: RequestOptions & { readonly deviceId?: string } = {}): Promise<void> {
    let deviceId: string | undefined;
    if (options.deviceId !== undefined) {
      try { deviceId = canonicalUuid(options.deviceId); }
      catch { throw new ClientError("request", "Expected a non-nil device ID", "not_sent"); }
    }
    await this.#request("sign-out", {}, options, "unknown", deviceId);
  }

  session(grant: BrowserDeviceSession, options: RequestOptions = {}): Promise<SharedSession> {
    return this.#session(grant.device, options);
  }

  connect(grant: BrowserDeviceSession, send?: Fetch): PairedDeviceClient {
    const device = grant.device;
    const { fetch, timeout, maxBytes } = this.#transport;
    const dispatch: Fetch = (input, init) => {
      const headers = new Headers(init?.headers);
      headers.set(deviceIdHeader, device.id);
      return (send ?? fetch)(input, { ...init, headers });
    };
    const http = new HttpClient({ endpoint: this.origin, cookie: "same-origin", contract, fetch: dispatch,
      requestTimeoutMs: timeout, maxResponseBytes: maxBytes });
    const notificationTransport = transportSettings({
      fetch: dispatch, requestTimeoutMs: timeout, maxResponseBytes: Math.min(8192, maxBytes),
    }, "same-origin");
    return new PairedDeviceClient(http, device, async (options) => {
      const current = await this.me(options);
      sameDevice(device, current.device, "not_applicable");
      return current.device;
    }, (options = {}) => this.#session(device, options), (route, options) =>
      requestJson(notificationTransport, `${this.origin}/devices/${route}`, {
        method: "GET", headers: { accept: "application/json" },
      }, options, "not_applicable"), { webPush: new BrowserPushClient(this.origin, notificationTransport) });
  }

  async #session({ id, scope }: DeviceRegistration, options: RequestOptions): Promise<SharedSession> {
    return sharedSession(await this.#request("session", undefined, options, "not_applicable", id), scope.session_id);
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

/** Paired Cookie transport for opting a browser installation into Web Push. */
export class BrowserPushClient {
  readonly #endpoint: string;
  readonly #transport: ReturnType<typeof transportSettings>;

  constructor(origin: string, transport: ReturnType<typeof transportSettings>) {
    this.#endpoint = `${origin}/devices/notifications/web-push`;
    this.#transport = transport;
  }

  async status(options: RequestOptions = {}): Promise<WebPushStatus> {
    return await this.#request(undefined, options);
  }

  async update(request: WebPushUpdate, options: RequestOptions = {}): Promise<WebPushStatus> {
    let captured: WebPushUpdate;
    try {
      const sub = request.subscription;
      if (sub !== null && (typeof sub.endpoint !== "string" || sub.endpoint.length > 1024 ||
          !/^B[A-Za-z0-9_-]{86}$/.test(sub.p256dh) || !/^[A-Za-z0-9_-]{22}$/.test(sub.auth) ||
          !/^B[A-Za-z0-9_-]{86}$/.test(sub.application_server_key))) throw new Error("Invalid push subscription");
      captured = { ...updateIdentity(request), subscription: sub === null ? null : {
        endpoint: sub.endpoint, p256dh: sub.p256dh, auth: sub.auth, application_server_key: sub.application_server_key,
      } };
    } catch {
      throw new ClientError("request", "Web Push update is invalid", "not_sent");
    }
    return await this.#request(captured, options);
  }

  async #request(body: WebPushUpdate | undefined, options: RequestOptions): Promise<WebPushStatus> {
    const headers = new Headers({ accept: "application/json" });
    if (body) headers.set("content-type", "application/json");
    const outcome = body ? "unknown" : "not_applicable";
    const response = await requestJson(this.#transport, this.#endpoint, body
      ? { method: "POST", headers, body: JSON.stringify(body) }
      : { method: "GET", headers }, options, outcome);
    try {
      const raw = object(response);
      const status = pushStatus(raw, body && { revision: body.expected_revision, enabled: body.subscription !== null });
      const { application_server_key } = raw;
      if (typeof application_server_key !== "string" || !/^B[A-Za-z0-9_-]{86}$/.test(application_server_key) ||
          body?.subscription && application_server_key !== body.subscription.application_server_key) {
        throw new Error("Invalid push application server key");
      }
      return { ...status, application_server_key };
    } catch {
      throw new ClientError("protocol", "Web Push status is invalid", outcome);
    }
  }
}

/** Notification subscription management on the paired host. */
export class NativePushClient {
  readonly #request: NativePushRequest;

  constructor(request: NativePushRequest) {
    this.#request = request;
  }

  async directStatus(platform: NativePushPlatform, options: RequestOptions = {}): Promise<NativePushStatus> {
    return this.#direct(platform, undefined, options);
  }

  async directUpdate(platform: NativePushPlatform, request: NativePushUpdate, options: RequestOptions = {}): Promise<NativePushStatus> {
    let body: NativePushUpdate;
    try {
      const sub = request.subscription;
      if (sub !== null && sub.platform !== platform) throw new Error("Mismatched push platform");
      body = { ...updateIdentity(request), subscription: sub === null ? null : {
        platform, channel: nativePushChannel(platform, sub.channel), device_token: pushToken(platform, sub.device_token),
      } };
    } catch { throw new ClientError("request", "Native push update is invalid", "not_sent"); }
    return this.#direct(platform, body, options);
  }

  async relayStatus(options: RequestOptions = {}): Promise<RelayPushStatus> {
    return this.#relay(undefined, options);
  }

  async relayUpdate(request: RelayPushUpdate, options: RequestOptions = {}): Promise<RelayPushStatus> {
    let body: RelayPushUpdate;
    try {
      const sub = request.subscription;
      if (sub !== null && (typeof sub.capability !== "string" || sub.capability.length < 16 ||
          sub.capability.length > 512 || /[^A-Za-z0-9_.~-]/.test(sub.capability))) {
        throw new Error("Invalid relay capability");
      }
      body = { ...updateIdentity(request), subscription: sub === null ? null : {
        relay_origin: relayOrigin(sub.relay_origin), capability: sub.capability,
      } };
    } catch { throw new ClientError("request", "Push relay subscription update is invalid", "not_sent"); }
    return this.#relay(body, options);
  }

  async #direct(platform: NativePushPlatform, body: NativePushUpdate | undefined, options: RequestOptions): Promise<NativePushStatus> {
    checkPushPlatform(platform);
    return this.#read(`notifications/native/${platform}`, body, options, raw => {
      const channel = nativePushChannel(platform, raw.channel);
      if (body?.subscription && channel !== body.subscription.channel) throw new Error("Native push channel changed");
      return { channel };
    });
  }

  async #relay(body: RelayPushUpdate | undefined, options: RequestOptions): Promise<RelayPushStatus> {
    return this.#read("notifications/subscription", body, options, raw => {
      const relay_origin = relayOrigin(raw.relay_origin);
      if (raw.relay_origin !== relay_origin || body?.subscription && relay_origin !== body.subscription.relay_origin) {
        throw new Error("Relay origin changed");
      }
      return { relay_origin };
    });
  }

  async #read<T>(route: NativePushRoute, body: NativePushUpdate | RelayPushUpdate | undefined, options: RequestOptions,
    decodeChannel: (raw: Record<string, unknown>) => T): Promise<PushStatus & T> {
    const reply = await this.#request(route, body, options);
    try {
      const raw = object(reply);
      return { ...pushStatus(raw, body && { revision: body.expected_revision, enabled: body.subscription !== null }),
        ...decodeChannel(raw) };
    } catch {
      throw new ClientError("protocol", "Native notification subscription response is invalid", body ? "unknown" : "not_applicable");
    }
  }
}

/** Manage a single registration on a confirmed relay with its separate owner key. */
export class PushRelayClient {
  readonly #endpoint: string;
  readonly #registrationId: string;
  readonly #ownerKey: string;
  readonly #transport: ReturnType<typeof transportSettings>;

  constructor(options: HttpTransportOptions & { origin: string; registrationId: string; ownerKey: string }) {
    try {
      this.#registrationId = uuid(options.registrationId);
      if (!ownerKey(options.ownerKey)) throw new Error("Invalid relay owner key");
      this.#ownerKey = options.ownerKey;
      this.#endpoint = `${relayOrigin(options.origin)}/v1/registrations/${this.#registrationId}`;
    } catch { throw new ClientError("request", "Push relay registration identity is invalid", "not_sent"); }
    const transport = transportSettings(options);
    this.#transport = { ...transport, maxBytes: Math.min(4096, transport.maxBytes) };
  }

  async status(options: RequestOptions = {}): Promise<RelayRegistration> {
    return this.#request(undefined, options);
  }

  async update(request: RelayRegistrationUpdate, options: RequestOptions = {}): Promise<RelayRegistration> {
    let body: RelayRegistrationUpdate;
    try {
      const { platform, device_token } = request;
      checkPushPlatform(platform);
      body = { ...updateIdentity(request), platform, device_token: device_token === null ? null : pushToken(platform, device_token) };
    } catch { throw new ClientError("request", "Push relay registration update is invalid", "not_sent"); }
    return this.#request(body, options);
  }

  async #request(body: RelayRegistrationUpdate | undefined, options: RequestOptions): Promise<RelayRegistration> {
    const headers = new Headers({ accept: "application/json", authorization: `Bearer ${this.#ownerKey}` });
    if (body) headers.set("content-type", "application/json");
    const outcome = body ? "unknown" : "not_applicable";
    const response = await requestJson(this.#transport, this.#endpoint, body
      ? { method: "POST", headers, body: JSON.stringify(body) }
      : { method: "GET", headers }, options, outcome);
    try {
      const raw = object(response);
      const revision = uuid(raw.revision);
      const { capability, expires_at_unix } = raw;
      const prefix = `push-v1.${this.#registrationId}.`;
      if (!(capability === null || typeof capability === "string" &&
          capability.startsWith(prefix) && ownerKey(capability.slice(prefix.length))) ||
          typeof expires_at_unix !== "number" || !Number.isSafeInteger(expires_at_unix) || expires_at_unix <= 0 ||
          body && (revision === body.expected_revision || (capability !== null) !== (body.device_token !== null))) {
        throw new Error("Relay registration changed identity");
      }
      return { revision, capability, expires_at_unix };
    } catch { throw new ClientError("protocol", "Push relay registration response is invalid", outcome); }
  }
}

/** Authenticated access to the Session recorded in the device grant. */
class PairedDeviceClient {
  readonly #http: HttpClient;
  readonly #session: SessionClient;
  readonly #lookupNotification: (route: NotificationRoute, options: RequestOptions) => Promise<unknown>;
  readonly webPush: BrowserPushClient | undefined;
  readonly nativePush: NativePushClient | undefined;

  constructor(http: HttpClient, readonly device: DeviceRegistration,
    readonly me: (options?: RequestOptions) => Promise<DeviceRegistration>,
    readonly session: (options?: RequestOptions) => Promise<SharedSession>,
    lookupNotification: (route: NotificationRoute, options: RequestOptions) => Promise<unknown>,
    push: { webPush?: BrowserPushClient; nativePush?: NativePushClient }) {
    this.#http = http;
    this.#session = new SessionClient(http, device.scope.session_id);
    this.#lookupNotification = lookupNotification;
    this.webPush = push.webPush;
    this.nativePush = push.nativePush;
    Object.freeze(this);
  }

  capabilities(options: RequestOptions = {}) { return this.#http.capabilities(options); }

  flows(request: Pick<Dtos["ListFlowsRequest"], "before" | "limit"> = {}, options: RequestOptions = {}) {
    return this.#session.flows(request, options);
  }

  flow(instanceId: string): FlowClient {
    return new FlowClient(this.#http, { session_id: this.device.scope.session_id, instance_id: instanceId }, "receipts");
  }

  /** Resolve an opaque notification using this device's current authorization. */
  async notification(id: string, options: RequestOptions = {}): Promise<DeviceNotification> {
    let notice: string;
    try { notice = canonicalUuid(id); }
    catch { throw new ClientError("request", "Expected a non-nil notification ID", "not_sent"); }
    return notificationDestination(await this.#lookupNotification(`notifications/${notice}`, options), this.device);
  }
}

export type { PairedDeviceClient };
