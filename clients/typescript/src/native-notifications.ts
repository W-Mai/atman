import type { Fetch, RequestOptions } from "./client.ts";
import { PushRelayClient, pushToken } from "./devices.ts";
import type { NativePushClient, NativePushPlatform, NativePushUpdate, RelayPushUpdate,
  RelayRegistration, RelayRegistrationUpdate } from "./devices.ts";
import { ClientError } from "./errors.ts";

type DirectTarget = Readonly<{ kind: "direct"; platform: NativePushPlatform; channel: string }>;
type RelayTarget = Readonly<{ kind: "relay"; platform: NativePushPlatform; origin: string }>;
export type NativeNotificationTarget = DirectTarget | RelayTarget;

type Active = Readonly<{ phase: "active"; token: string; revision: string }>;
type RegisteredRelay = RelayRegistration & Readonly<{ capability: string }>;
type DirectEnrollment = DirectTarget & Readonly<{
  state: Active | { phase: "direct"; request: NativePushUpdate };
}>;
type RelayEnrollment = RelayTarget & Readonly<{
  registration_id: string;
  owner_key: string;
  state:
    | (Active & { relay: RegisteredRelay })
    | { phase: "register"; request: RelayRegistrationUpdate; hostRevision: string | null }
    | { phase: "attach"; token: string; relay: RegisteredRelay; request: RelayPushUpdate }
    | { phase: "stopRegistration"; request: RelayRegistrationUpdate }
    | { phase: "detach"; relayRevision: string | null; request: RelayPushUpdate }
    | { phase: "retire"; request: RelayRegistrationUpdate };
}>;

/** Store only in the secure record of the device that owns these operations. */
export type NativeNotificationRecord = DirectEnrollment | RelayEnrollment;

export type NativeNotificationState = Readonly<
  { status: "off" } |
  { status: "on"; target: NativeNotificationTarget } |
  { status: "pending"; target: NativeNotificationTarget; change: "enable" | "disable" }
>;

type EnrollmentStore = Readonly<{
  signal: AbortSignal;
  read: () => Promise<NativeNotificationRecord | undefined>;
  write: (record: NativeNotificationRecord | undefined) => Promise<void>;
}>;

const renewalWindowSeconds = 7 * 86_400;

function httpStatus(error: unknown, ...codes: number[]): boolean {
  return error instanceof ClientError && error.status !== undefined && codes.includes(error.status);
}

function targetOf(record: NativeNotificationRecord): NativeNotificationTarget {
  if (record.kind === "direct") return { kind: "direct", platform: record.platform, channel: record.channel };
  return { kind: "relay", platform: record.platform, origin: record.origin };
}

function snapshot(record: NativeNotificationRecord | undefined): NativeNotificationState {
  if (record === undefined) return { status: "off" };
  const target = targetOf(record);
  if (record.state.phase === "active") return { status: "on", target };
  const disable = record.state.phase === "direct" ? record.state.request.subscription === null
    : record.state.phase === "detach" || record.state.phase === "retire" || record.state.phase === "stopRegistration";
  return { status: "pending", target, change: disable ? "disable" : "enable" };
}

function changed(message: string): ClientError {
  return new ClientError("source", message, "not_sent");
}

function registrationIdentity(): { registration_id: string; owner_key: string } {
  const bytes = crypto.getRandomValues(new Uint8Array(32));
  const owner_key = btoa(String.fromCharCode(...bytes)).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
  return { registration_id: crypto.randomUUID(), owner_key };
}

function active(token: string, revision: string | null): Active {
  if (!revision) throw new ClientError("protocol", "The notification update did not return a revision", "unknown");
  return { phase: "active", token, revision };
}

/** Own one native device's durable registration and its exact pending requests. */
export class NativeNotificationEnrollment {
  #queue: Promise<void> = Promise.resolve();
  readonly #options: RequestOptions;

  constructor(private readonly host: NativePushClient, private readonly store: EnrollmentStore,
    private readonly fetchRelay: Fetch | undefined) {
    this.#options = { signal: store.signal };
  }

  async state(): Promise<NativeNotificationState> {
    return snapshot(await this.store.read());
  }

  /** The caller captures an explicit choice and an authorized platform token. */
  enable(target: NativeNotificationTarget, token: string): Promise<NativeNotificationState> {
    const captured = structuredClone(target);
    return this.#run(async () => {
      if (await this.store.read()) throw changed("Turn off the current notification subscription first");
      return this.#enable(captured, token);
    });
  }

  /** A token observation only updates an opted-in registration. */
  updateToken(token: string): Promise<NativeNotificationState> {
    return this.#run(async () => {
      const record = await this.#finish(await this.store.read());
      if (!record || record.state.phase !== "active") return snapshot(record);
      if (token === record.state.token && (record.kind === "direct" ||
          record.state.relay.expires_at_unix > Date.now() / 1000 + renewalWindowSeconds)) return snapshot(record);
      return this.#enable(targetOf(record), token, record);
    });
  }

  disable(): Promise<NativeNotificationState> {
    return this.#run(async () => {
      const record = await this.store.read();
      if (!record) return snapshot(undefined);
      if (record.kind === "direct") {
        const { state } = record;
        if (state.phase === "direct" && state.request.subscription === null) {
          try { return snapshot(await this.#finish(record)); }
          catch (error) { if (!httpStatus(error, 409)) throw error; }
        }
        const current = await this.host.directStatus(record.platform, this.#options);
        if (current.enabled && current.channel !== record.channel) {
          throw changed("The active notification channel changed");
        }
        // An unacknowledged enable may still commit. Fence it with a disable at the same revision.
        const pendingEnable = state.phase === "direct" && state.request.subscription !== null &&
          state.request.expected_revision === current.revision;
        const maskedByConfig = state.phase === "active" && current.channel !== record.channel &&
          current.revision === state.revision;
        if (!current.enabled && !pendingEnable && !maskedByConfig) {
          await this.store.write(undefined);
          return snapshot(undefined);
        }
        return this.#saveAndFinish({ ...record, state: { phase: "direct", request: {
          request_id: crypto.randomUUID(), expected_revision: current.revision, subscription: null,
        } } });
      }
      const { state } = record;
      switch (state.phase) {
        case "register":
          return this.#saveAndFinish({ ...record, state: { phase: "stopRegistration", request: state.request } });
        case "stopRegistration":
        case "retire":
          return snapshot(await this.#finish(record));
        case "detach": {
          let next: RelayEnrollment | undefined;
          try { next = await this.#advanceRelay(record); }
          catch (error) {
            if (!httpStatus(error, 409)) throw error;
            return this.#finishRelayStop(record, state.relayRevision);
          }
          return this.#saveAndFinish(next);
        }
        case "attach":
          return this.#finishRelayStop(record, state.relay.revision, state.request.expected_revision);
        case "active":
          return this.#finishRelayStop(record, state.relay.revision);
      }
    });
  }

  /** Resume an uncertain write with the exact identity and body already in secure storage. */
  retry(): Promise<NativeNotificationState> {
    return this.#run(async () => snapshot(await this.#finish(await this.store.read())));
  }

  async #enable(target: NativeNotificationTarget, token: string,
    previous?: NativeNotificationRecord): Promise<NativeNotificationState> {
    pushToken(target.platform, token);
    if (target.kind === "direct") {
      const current = await this.host.directStatus(target.platform, this.#options);
      if (target.channel !== current.channel) throw changed("The host's native notification channel changed");
      this.#checkRevision(current.revision, previous);
      return this.#saveAndFinish({ ...target, state: { phase: "direct", request: {
        request_id: crypto.randomUUID(), expected_revision: current.revision,
        subscription: { platform: target.platform, channel: target.channel, device_token: token },
      } } });
    }
    const current = await this.host.relayStatus(this.#options);
    if (target.origin !== current.relay_origin) throw changed("The host's notification relay changed");
    if (!this.fetchRelay) throw changed("Native notification relay transport is unavailable");
    this.#checkRevision(current.revision, previous);
    const existing = previous?.kind === "relay" && previous.state.phase === "active" &&
      previous.state.relay.expires_at_unix > Date.now() / 1000 ? previous : undefined;
    const identity = existing
      ? { registration_id: existing.registration_id, owner_key: existing.owner_key }
      : registrationIdentity();
    const relay = existing?.state.phase === "active" ? existing.state.relay : undefined;
    return this.#saveAndFinish({ ...target, ...identity, state: {
      phase: "register", hostRevision: current.revision,
      request: { request_id: crypto.randomUUID(), expected_revision: relay?.revision ?? null,
        platform: target.platform, device_token: token },
    } });
  }

  #checkRevision(revision: string | null, record: NativeNotificationRecord | undefined): void {
    if (record?.state.phase === "active" && revision !== record.state.revision) {
      throw changed("The active notification subscription changed");
    }
  }

  async #saveAndFinish(record: NativeNotificationRecord | undefined): Promise<NativeNotificationState> {
    await this.store.write(record);
    return snapshot(await this.#finish(record));
  }

  async #finish(record: NativeNotificationRecord | undefined): Promise<NativeNotificationRecord | undefined> {
    while (record && record.state.phase !== "active") {
      const next = record.kind === "direct" ? await this.#advanceDirect(record) : await this.#advanceRelay(record);
      await this.store.write(next);
      record = next;
    }
    return record;
  }

  async #advanceDirect(record: DirectEnrollment): Promise<DirectEnrollment | undefined> {
    const { state, platform } = record;
    if (state.phase !== "direct") throw changed("The direct notification operation changed");
    const result = await this.host.directUpdate(platform, state.request, this.#options);
    return state.request.subscription ? { ...record, state: active(state.request.subscription.device_token, result.revision) } : undefined;
  }

  async #advanceRelay(record: RelayEnrollment): Promise<RelayEnrollment | undefined> {
    const { state } = record;
    switch (state.phase) {
      case "register": {
        const relay = await this.#relay(record).update(state.request, this.#options);
        const token = state.request.device_token;
        if (relay.capability === null || token === null) {
          throw new ClientError("protocol", "The relay did not confirm the registration", "unknown");
        }
        const registered = { ...relay, capability: relay.capability };
        return { ...record, state: { phase: "attach", token, relay: registered, request: {
          request_id: crypto.randomUUID(), expected_revision: state.hostRevision,
          subscription: { relay_origin: record.origin, capability: relay.capability },
        } } };
      }
      case "attach": {
        const result = await this.host.relayUpdate(state.request, this.#options);
        return { ...record, state: { ...active(state.token, result.revision), relay: state.relay } };
      }
      case "stopRegistration": {
        const relay = this.#relay(record);
        const pending = await relay.update(state.request, this.#options).catch((error: unknown) => {
          if (!httpStatus(error, 409)) throw error;
          return relay.status(this.#options);
        }).catch((error: unknown) => {
          if (!httpStatus(error, 404, 410)) throw error;
          return undefined;
        });
        return this.#relayStopStep(record, pending?.revision ?? null);
      }
      case "detach":
        await this.host.relayUpdate(state.request, this.#options);
        return this.#retire(record, state.relayRevision);
      case "retire": {
        const relay = this.#relay(record);
        try { await relay.update(state.request, this.#options); }
        catch (error) {
          if (httpStatus(error, 409)) {
            const current = await relay.status(this.#options).catch((failure: unknown) => {
              if (!httpStatus(failure, 404, 410)) throw failure;
              return undefined;
            });
            if (current?.capability != null) throw error;
          } else if (!httpStatus(error, 404, 410)) throw error;
        }
        return undefined;
      }
      case "active": throw changed("The relay notification operation changed");
    }
  }

  #retire(record: RelayEnrollment, revision: string | null): RelayEnrollment | undefined {
    if (revision === null) return undefined;
    return { ...record, state: { phase: "retire", request: {
      request_id: crypto.randomUUID(), expected_revision: revision, platform: record.platform, device_token: null,
    } } };
  }

  async #relayStopStep(record: RelayEnrollment, relayRevision: string | null,
    attachingAt?: string | null): Promise<RelayEnrollment | undefined> {
    const current = await this.host.relayStatus(this.#options).catch((error: unknown) => {
      if (!httpStatus(error, 404)) throw error;
      return undefined;
    });
    const mustFence = attachingAt !== undefined && attachingAt === current?.revision;
    if (current?.relay_origin === record.origin && (current.enabled || mustFence)) {
      return { ...record, state: { phase: "detach", relayRevision, request: {
        request_id: crypto.randomUUID(), expected_revision: current.revision, subscription: null,
      } } };
    }
    return this.#retire(record, relayRevision);
  }

  async #finishRelayStop(record: RelayEnrollment, revision: string | null,
    attachingAt?: string | null): Promise<NativeNotificationState> {
    return this.#saveAndFinish(await this.#relayStopStep(record, revision, attachingAt));
  }

  #relay(record: RelayEnrollment): PushRelayClient {
    if (!this.fetchRelay) throw changed("Native notification relay transport is unavailable");
    return new PushRelayClient({ origin: record.origin, registrationId: record.registration_id,
      ownerKey: record.owner_key, fetch: this.fetchRelay });
  }

  #run<T>(work: () => Promise<T>): Promise<T> {
    const operation = this.#queue.then(work);
    this.#queue = operation.then(() => {}, () => {});
    return operation;
  }
}
