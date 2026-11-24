import { registerPlugin } from "@capacitor/core";
import type { PluginListenerHandle } from "@capacitor/core";
import { canonicalUuid } from "../src/index.ts";
import type { Fetch, NativeDeviceStore, NativePushPlatform, StoredDevice } from "../src/index.ts";
import type { NativeNoticeTap } from "../web/device-workspace.ts";

type BeginRequest = (options: { id: string; url: string; method: string; headers: Record<string, string>; body?: string }) =>
  Promise<{ status: number; contentType?: string }>;

export type PlatformNotificationStatus = Readonly<{
  platform: NativePushPlatform;
  channel: string | null;
  permission: "prompt" | "granted" | "denied";
}>;

interface DeviceBridge {
  scanInvitation(): Promise<{ link: string | null }>;
  loadRecord(): Promise<{ record: string | null }>;
  replaceRecord(options: { record: string | null }): Promise<void>;
  begin: BeginRequest;
  beginRelay: BeginRequest;
  read(options: { id: string }): Promise<{ bytes: string | null }>;
  cancel(options: { id: string }): Promise<void>;
  pendingNotification(): Promise<{ notice: NativeNoticeTap | null }>;
  acknowledgeNotification(options: { ticket: string }): Promise<void>;
  notificationStatus(): Promise<PlatformNotificationStatus>;
  notificationToken(options: { device: string; prompt: boolean }): Promise<{ token: string | null }>;
  stopNotificationToken(options: { device: string }): Promise<void>;
  addListener(event: "notificationOpen" | "notificationTokenChanged", notify: () => void): Promise<PluginListenerHandle>;
}

const bridge = registerPlugin<DeviceBridge>("AtmanDevice");

export async function scanInvitation(): Promise<string | null> {
  return (await bridge.scanInvitation()).link;
}

export const notificationTaps = {
  async pending(): Promise<NativeNoticeTap | undefined> {
    const { notice } = await bridge.pendingNotification();
    return notice ? {
      notification_id: canonicalUuid(notice.notification_id),
      device_id: canonicalUuid(notice.device_id),
      ticket: canonicalUuid(notice.ticket),
    } : undefined;
  },
  acknowledge(ticket: string): Promise<void> {
    return bridge.acknowledgeNotification({ ticket });
  },
  async watch(notify: () => void): Promise<() => Promise<void>> {
    const listener = await bridge.addListener("notificationOpen", notify);
    return () => listener.remove();
  },
};

export const notificationPlatform = {
  status: () => bridge.notificationStatus(),
  async token(device: string, prompt: boolean): Promise<string | null> {
    return (await bridge.notificationToken({ device, prompt })).token;
  },
  stop: (device: string) => bridge.stopNotificationToken({ device }),
  async watch(notify: () => void): Promise<() => Promise<void>> {
    const listener = await bridge.addListener("notificationTokenChanged", notify);
    return () => listener.remove();
  },
};
export const deviceStore: NativeDeviceStore = {
  async load() {
    const { record } = await bridge.loadRecord();
    return record === null ? undefined : JSON.parse(record) as StoredDevice;
  },
  replace(value) {
    return bridge.replaceRecord({ record: value === undefined ? null : JSON.stringify(value) });
  },
};

const bridgeFetch = (begin: BeginRequest): Fetch => async (input, options) => {
  const signal = options?.signal;
  signal?.throwIfAborted();
  const id = crypto.randomUUID();
  const body = options?.body;
  if (body != null && typeof body !== "string") throw new Error("Expected a JSON request body");
  const headers = Object.fromEntries(new Headers(options?.headers));
  const cancel = () => { void bridge.cancel({ id }).catch(() => {}); };
  const retire = () => signal?.removeEventListener("abort", cancel);
  signal?.addEventListener("abort", cancel, { once: true });
  try {
    const response = await begin({ id, url: input instanceof Request ? input.url : String(input),
      method: options?.method ?? "GET", headers, ...(body == null ? {} : { body }) });
    signal?.throwIfAborted();
    const metadata = { status: response.status, headers: response.contentType ? { "content-type": response.contentType } : {} };
    if ([204, 205, 304].includes(response.status)) {
      retire();
      cancel();
      return new Response(null, metadata);
    }
    return new Response(new ReadableStream<Uint8Array>({
      async pull(controller) {
        try {
          const { bytes } = await bridge.read({ id });
          if (bytes === null) { retire(); controller.close(); }
          else controller.enqueue(Uint8Array.from(atob(bytes), value => value.charCodeAt(0)));
        } catch (error) {
          retire();
          cancel();
          throw error;
        }
      },
      cancel() { retire(); cancel(); },
    }, { highWaterMark: 0 }), metadata);
  } catch (error) {
    retire();
    cancel();
    throw error;
  }
};

export const fetchDevice = bridgeFetch(options => bridge.begin(options));
export const fetchRelay = bridgeFetch(options => bridge.beginRelay(options));
