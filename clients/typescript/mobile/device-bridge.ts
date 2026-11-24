import { registerPlugin } from "@capacitor/core";
import type { Fetch, NativeDeviceStore, StoredDevice } from "../src/index.ts";

interface DeviceBridge {
  loadRecord(): Promise<{ record: string | null }>;
  replaceRecord(options: { record: string | null }): Promise<void>;
  begin(options: { id: string; url: string; method: string; headers: Record<string, string>; body?: string }):
    Promise<{ status: number; contentType?: string }>;
  read(options: { id: string }): Promise<{ bytes: string | null }>;
  cancel(options: { id: string }): Promise<void>;
}

const bridge = registerPlugin<DeviceBridge>("AtmanDevice");

export const deviceStore: NativeDeviceStore = {
  async load() {
    const { record } = await bridge.loadRecord();
    return record === null ? undefined : JSON.parse(record) as StoredDevice;
  },
  replace(value) {
    return bridge.replaceRecord({ record: value === undefined ? null : JSON.stringify(value) });
  },
};

export const fetchDevice: Fetch = async (input, options) => {
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
    const response = await bridge.begin({ id, url: input instanceof Request ? input.url : String(input),
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
