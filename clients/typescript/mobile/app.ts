import { mount } from "svelte";
import { NativeDeviceSession } from "../src/index.ts";
import Workbench from "../web/Workbench.svelte";
import { bindDeviceWorkbench } from "../web/device-workspace.ts";
import { deviceStore, fetchDevice, fetchRelay, notificationTaps, scanInvitation } from "./device-bridge.ts";
import "../web/app.css";
import "./app.css";

const target = document.querySelector("#app");
if (!target) throw new Error("Missing workbench mount point");
if (!crypto.randomUUID) {
  Object.defineProperty(crypto, "randomUUID", { value: () => {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    bytes[6] = (bytes[6]! & 15) | 64;
    bytes[8] = (bytes[8]! & 63) | 128;
    const hex = Array.from(bytes, value => value.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  } });
}
const session = new NativeDeviceSession(deviceStore, { fetch: fetchDevice, relayFetch: fetchRelay });
mount(Workbench, { target, props: {
  onReady: (elements) => bindDeviceWorkbench(elements, { session, scanInvitation, notificationTaps }),
} });
