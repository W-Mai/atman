import { ClientError } from "../src/index.ts";
import type { NativeNotificationEnrollment, NativeNotificationState, NativeNotificationTarget, NativePushClient } from "../src/index.ts";
import { notificationDialog } from "../web/notification-dialog.ts";
import { notificationPlatform } from "./device-bridge.ts";
import type { PlatformNotificationStatus } from "./device-bridge.ts";

function ifAvailable<T>(request: Promise<T>): Promise<T | undefined> {
  return request.catch((error: unknown) => {
    if (error instanceof ClientError && error.status === 404) return undefined;
    throw error;
  });
}

function delivery(target: NativeNotificationTarget): string {
  if (target.kind === "relay") return target.origin;
  return target.platform === "apns" ? "Direct · APNs" : "Direct · FCM";
}

export function bindNativeNotifications(button: HTMLButtonElement, device: string, host: NativePushClient,
  enrollment: NativeNotificationEnrollment, platform = notificationPlatform) {
  const life = new AbortController();
  const options = { signal: life.signal };
  const { dialog, message, actions, enable, disable, close } = notificationDialog(button);
  dialog.classList.add("native-notifications");
  const choice = document.createElement("select");
  choice.setAttribute("aria-label", "Notification delivery");
  const selectedRoute = document.createElement("p");
  selectedRoute.className = "dialog-copy";
  actions.before(choice, selectedRoute);

  let os: PlatformNotificationStatus | undefined;
  let state: NativeNotificationState = { status: "off" };
  let offered: readonly NativeNotificationTarget[] = [];
  let pending: Promise<void> | undefined;
  let pendingLabel = "";
  let queuedCheck = false;
  let signingOut = false;
  let failure = "";

  function render(): void {
    if (life.signal.aborted) return;
    const active = state.status !== "off";
    button.hidden = !active && offered.length === 0 && !(os?.channel && failure);
    dialog.setAttribute("aria-busy", String(Boolean(pending) || signingOut));
    message.textContent = failure || (signingOut ? "Turning off…" :
      pending && dialog.open ? pendingLabel :
      os?.permission === "denied" ? "Allow notifications in system settings." :
      state.status !== "off" && os && !matches(state.target, os) ? "Delivery configuration changed." :
      state.status === "pending" ? "Change pending" : state.status === "on" ? "On" :
      offered.length ? "Off" : "Notifications are unavailable.");
    choice.hidden = active || offered.length < 2;
    choice.disabled = Boolean(pending) || signingOut;
    selectedRoute.hidden = !active && offered.length !== 1;
    selectedRoute.textContent = state.status === "off" ? (offered[0] ? delivery(offered[0]) : "") : delivery(state.target);
    enable.hidden = state.status === "on" || state.status === "off" && offered.length === 0;
    enable.textContent = state.status === "pending" ? "Retry" : "Turn on";
    const blocked = state.status === "pending"
      ? state.change === "enable" && (!os || os.permission !== "granted" || !matches(state.target, os))
      : !os?.channel || os.permission === "denied";
    enable.disabled = Boolean(pending) || signingOut || blocked;
    disable.hidden = !active;
    disable.disabled = Boolean(pending) || signingOut;
  }

  function run(label: string, work: () => Promise<void>): Promise<void> {
    if (pending || signingOut || life.signal.aborted) return pending ?? Promise.resolve();
    failure = "";
    pendingLabel = label;
    const running = Promise.resolve().then(work).catch(async (error: unknown) => {
      if (life.signal.aborted) return;
      state = await enrollment.state().catch(() => state);
      failure = state.status === "pending" ? "Couldn’t confirm the change." :
        error instanceof ClientError && error.kind === "source" ? error.message : "Couldn’t update notifications.";
    }).finally(() => {
      pending = undefined;
      render();
      if (queuedCheck) { queuedCheck = false; void check(); }
    });
    pending = running;
    render();
    return running;
  }

  async function readChoices(current: PlatformNotificationStatus): Promise<void> {
    if (!current.channel) {
      offered = [];
      choice.replaceChildren();
      return;
    }
    const [direct, relay] = await Promise.all([
      ifAvailable(host.directStatus(current.platform, options)),
      ifAvailable(host.relayStatus(options)),
    ]);
    const next: NativeNotificationTarget[] = [];
    if (direct?.channel === current.channel) next.push({ kind: "direct", platform: current.platform, channel: direct.channel });
    if (relay) next.push({ kind: "relay", platform: current.platform, origin: relay.relay_origin });
    const selected = offered[choice.selectedIndex];
    offered = next;
    choice.replaceChildren(...offered.map(target => new Option(delivery(target))));
    const previous = selected && offered.findIndex(target => target.kind === selected.kind && delivery(target) === delivery(selected));
    if (previous !== undefined && previous >= 0) choice.selectedIndex = previous;
  }

  function check(): Promise<void> {
    if (pending) { queuedCheck = true; return pending; }
    return run("Checking…", async () => {
      [state, os] = await Promise.all([enrollment.state(), platform.status()]);
      if (life.signal.aborted || signingOut) return;
      if (state.status !== "off" && os.permission === "denied" &&
          (state.status === "on" || state.change === "enable")) {
        state = await enrollment.disable();
      } else if (state.status === "pending" &&
          (state.change === "disable" || os.permission === "granted" && matches(state.target, os))) {
        state = await enrollment.retry();
      }
      if (state.status === "on" && os.permission === "granted" && matches(state.target, os)) {
        const token = await platform.token(device, false);
        if (token !== null && !life.signal.aborted && !signingOut) state = await enrollment.updateToken(token);
      }
      if (state.status === "off") await readChoices(os);
    });
  }

  function matches(target: NativeNotificationTarget, current: PlatformNotificationStatus): boolean {
    return target.platform === current.platform && current.channel !== null &&
      (target.kind === "relay" || target.channel === current.channel);
  }

  function turnOn(): Promise<void> {
    const target = offered[choice.selectedIndex];
    return run(state.status === "pending" ? "Retrying…" : "Turning on…", async () => {
      if (state.status === "pending") {
        if (state.change === "disable" || os?.permission === "granted" && matches(state.target, os)) {
          state = await enrollment.retry();
          queuedCheck = true;
        }
        return;
      }
      if (state.status === "on" || !target || !os || !matches(target, os)) return;
      try {
        const token = await platform.token(device, true);
        os = await platform.status();
        if (token === null || life.signal.aborted || signingOut) return;
        state = await enrollment.enable(target, token);
        queuedCheck = true;
      } catch (error) {
        if ((await enrollment.state().catch(() => state)).status === "off") {
          await platform.stop(device).catch(() => {});
        }
        throw error;
      }
    });
  }

  button.addEventListener("click", () => {
    if (!dialog.open) dialog.showModal();
    void check();
  }, options);
  enable.addEventListener("click", () => void turnOn(), options);
  disable.addEventListener("click", () => void run("Turning off…", async () => {
    state = await enrollment.disable();
    if (os) await readChoices(os);
  }), options);
  close.addEventListener("click", () => dialog.close(), options);
  window.addEventListener("online", () => void check(), options);
  void platform.watch(() => void check()).then(unwatch => {
    if (life.signal.aborted) void unwatch().catch(() => {});
    else life.signal.addEventListener("abort", () => void unwatch().catch(() => {}), { once: true });
  }).catch(() => {});
  render();
  void check();

  return {
    check,
    async beforeSignOut() {
      signingOut = true;
      render();
      try {
        await pending;
        state = await enrollment.disable();
        if (dialog.open) dialog.close();
      } finally { signingOut = false; render(); }
    },
    destroy() { life.abort(); dialog.remove(); button.hidden = true; },
  };
}
