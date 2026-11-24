import { canonicalUuid } from "./cursor.ts";
import { DeviceClient } from "./devices.ts";
import type { DeviceClientOptions, DeviceCredentials, PairedDeviceClient } from "./devices.ts";
import { ClientError } from "./errors.ts";
import type { RequestOptions } from "./client.ts";

export type StoredDevice = Readonly<{
  credentials: DeviceCredentials;
  renewal?: string;
}>;

/** One atomic secure record per installation. A failed write must retain a readable record. */
export interface NativeDeviceStore {
  load(): Promise<StoredDevice | undefined>;
  replace(value: StoredDevice | undefined): Promise<void>;
}

type Binding = {
  readonly auth: DeviceClient;
  readonly client: PairedDeviceClient;
  readonly lifetime: AbortController;
  value: StoredDevice;
};

const renewalMargin = 30_000;

function unauthorized(error: unknown): boolean {
  return error instanceof ClientError && error.status === 401;
}

function validSignal(signal: AbortSignal | null | undefined): void {
  if (signal?.aborted) throw new ClientError("aborted", "Device request was cancelled", "not_sent");
}

function expiry(value: StoredDevice): number {
  return Date.parse(value.credentials.access_expires_at);
}

function sameGrant(left: DeviceCredentials, right: DeviceCredentials): boolean {
  return left.origin === right.origin && left.device.id === right.device.id &&
    left.device.scope.session_id === right.device.scope.session_id &&
    left.device.scope.permission === right.device.scope.permission;
}

/** Own the secure record and the lifetime of the native installation's paired SDK connection. */
export class NativeDeviceSession {
  #binding: Binding | undefined;
  #queue: Promise<void> = Promise.resolve();

  constructor(private readonly store: NativeDeviceStore,
    private readonly options: Omit<DeviceClientOptions, "origin"> = {}) {}

  pair(origin: string, invitation: string, options: RequestOptions = {}): Promise<PairedDeviceClient> {
    return this.#serialized(async () => {
      validSignal(options.signal);
      const auth = new DeviceClient({ ...this.options, origin });
      const credentials = await auth.pair(invitation, options);
      const value = { credentials };
      await this.store.replace(value);
      return this.#install(auth, value).client;
    });
  }

  /** Reverify the installed grant on startup or foreground. An offline failure preserves the secure record. */
  resume(options: RequestOptions = {}): Promise<PairedDeviceClient | undefined> {
    return this.#serialized(async () => {
      validSignal(options.signal);
      const stored = await this.store.load();
      validSignal(options.signal);
      if (!stored) {
        this.#retire();
        return;
      }
      const value = structuredClone(stored);
      const auth = new DeviceClient({ ...this.options, origin: value.credentials.origin });
      const current = auth.connect(value.credentials);
      if (value.renewal !== undefined) canonicalUuid(value.renewal);
      let binding = this.#binding;
      if (binding && sameGrant(binding.value.credentials, value.credentials)) binding.value = value;
      else binding = this.#install(auth, value);
      if (!value.renewal && expiry(value) > Date.now() + renewalMargin) {
        try {
          await current.me(options);
          return binding.client;
        } catch (error) {
          if (!unauthorized(error)) throw error;
        }
      }
      await this.#renew(binding, options);
      return binding.client;
    });
  }

  /** Clear this installation's record and retire its requests. Host-side work remains with its owner. */
  signOut(): Promise<void> {
    return this.#serialized(async () => {
      await this.store.replace(undefined);
      this.#retire();
    });
  }

  #retire(): void {
    this.#binding?.lifetime.abort();
    this.#binding = undefined;
  }

  #install(auth: DeviceClient, value: StoredDevice): Binding {
    this.#retire();
    const lifetime = new AbortController();
    const send = this.options.fetch ?? globalThis.fetch.bind(globalThis);
    const client = new DeviceClient({ ...this.options, origin: auth.origin, fetch: async (input, init) => {
      const signal = AbortSignal.any(init?.signal ? [lifetime.signal, init.signal] : [lifetime.signal]);
      const current = await this.#serialized(async () => {
        validSignal(signal);
        if (binding.value.renewal || expiry(binding.value) <= Date.now() + renewalMargin) {
          await this.#renew(binding, { signal });
        }
        return binding.value.credentials;
      });
      validSignal(signal);
      const headers = new Headers(init?.headers);
      headers.set("authorization", `Bearer ${current.access_token}`);
      return send(input, { ...init, headers, signal });
    } }).connect(value.credentials);
    const binding = { auth, value, client, lifetime };
    this.#binding = binding;
    return binding;
  }

  async #renew(binding: Binding, options: RequestOptions): Promise<void> {
    validSignal(options.signal);
    const rotating = binding.value;
    await this.#rotate(binding, options);
    // The recovered rotation may itself have expired while the app was closed.
    if (rotating.renewal && expiry(binding.value) <= Date.now()) await this.#rotate(binding, options);
    if (expiry(binding.value) <= Date.now()) throw new ClientError("source", "The renewed device access has expired");
  }

  async #rotate(binding: Binding, options: RequestOptions): Promise<void> {
    validSignal(options.signal);
    const original = binding.value;
    const renewal = original.renewal ?? crypto.randomUUID();
    if (!original.renewal) {
      const pending = { credentials: original.credentials, renewal };
      await this.store.replace(pending);
      binding.value = pending;
    }
    let credentials: DeviceCredentials;
    try {
      credentials = await binding.auth.refresh(original.credentials, renewal, options);
    } catch (error) {
      if (unauthorized(error)) {
        await this.store.replace(undefined);
        this.#retire();
      }
      throw error;
    }
    const value = { credentials };
    await this.store.replace(value);
    binding.value = value;
  }

  #serialized<T>(work: () => Promise<T>): Promise<T> {
    const operation = this.#queue.then(work);
    this.#queue = operation.then(() => {}, () => {});
    return operation;
  }
}
