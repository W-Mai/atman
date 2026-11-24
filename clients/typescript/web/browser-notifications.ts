import { ClientError } from "../src/index.ts";
import type { BrowserPushClient, WebPushStatus, WebPushUpdate } from "../src/index.ts";

function keyBytes(encoded: string): Uint8Array<ArrayBuffer> {
  return Uint8Array.from(atob(encoded.replace(/-/g, "+").replace(/_/g, "/")), char => char.charCodeAt(0));
}

function usesKey(subscription: PushSubscription | null, key: Uint8Array): boolean {
  const value = subscription?.options.applicationServerKey;
  return value != null && new Uint8Array(value).every((byte, index) => byte === key[index]) && value.byteLength === key.length;
}

export function bindBrowserNotifications(button: HTMLButtonElement, api: BrowserPushClient) {
  const lifetime = new AbortController();
  const options = { signal: lifetime.signal };
  const supported = "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
  const dialog = document.createElement("dialog");
  dialog.className = "browser-notifications";
  dialog.setAttribute("aria-label", "Notifications");
  const title = document.createElement("h2");
  title.textContent = "Notifications";
  const message = document.createElement("p");
  message.className = "dialog-copy";
  message.setAttribute("role", "status");
  const actions = document.createElement("div");
  actions.className = "dialog-actions";
  const enable = document.createElement("button");
  enable.type = "button";
  enable.textContent = "Turn on";
  enable.className = "primary";
  const disable = document.createElement("button");
  disable.type = "button";
  disable.textContent = "Turn off";
  const close = document.createElement("button");
  close.type = "button";
  close.textContent = "Close";
  actions.append(close, disable, enable);
  dialog.append(title, message, actions);
  document.body.append(dialog);
  button.setAttribute("aria-label", "Notifications");
  button.title = "Notifications";

  let status: WebPushStatus | undefined;
  let registration: ServiceWorkerRegistration | undefined;
  let local: PushSubscription | null = null;
  let pending: WebPushUpdate | undefined;
  let changing = false;
  let preparing = false;
  let checking = false;
  let observation = 0;
  let failure = "";

  function render(): void {
    if (options.signal.aborted) return;
    button.hidden = status === undefined;
    const usable = status && usesKey(local, keyBytes(status.application_server_key));
    const denied = supported && Notification.permission === "denied";
    message.textContent = failure || (changing ? "Saving…" : preparing ? "Preparing browser…" :
      !supported ? "Install atman on a browser that supports Web Push." :
      denied ? "Notifications are blocked in this browser." :
      status?.enabled && usable ? "On" : status?.enabled ? "Browser subscription expired." : "Off");
    enable.textContent = pending ? "Retry" : "Turn on";
    enable.hidden = !pending && (!supported || Boolean(status?.enabled && usable));
    enable.disabled = changing || !status || (!pending && (!registration || denied));
    disable.hidden = !status?.enabled || Boolean(pending);
    disable.disabled = changing;
  }

  async function check(): Promise<void> {
    if (options.signal.aborted || changing || pending || checking) return;
    const observed = observation;
    checking = true;
    try {
      const result = await api.status(options);
      if (observation === observed) status = result;
    }
    catch (error) {
      if (observation === observed && error instanceof ClientError && error.status === 404) status = undefined;
    } finally {
      checking = false;
      render();
    }
  }

  async function prepare(): Promise<void> {
    if (!supported || preparing) return;
    preparing = true;
    render();
    let timeout: ReturnType<typeof setTimeout> | undefined;
    try {
      const worker = await navigator.serviceWorker.register("/notification-worker.js");
      registration = worker.active ? worker : await Promise.race([
        navigator.serviceWorker.ready,
        new Promise<never>((_, reject) => {
          timeout = setTimeout(() => reject(new Error("Worker did not activate")), 8_000);
        }),
      ]);
      local = await registration.pushManager.getSubscription();
    } catch { failure = "Couldn’t prepare this browser. Close and try again."; }
    finally {
      clearTimeout(timeout);
      preparing = false;
      render();
    }
  }

  async function change(turnOn: boolean): Promise<void> {
    if (!status || changing || options.signal.aborted) return;
    observation += 1;
    changing = true;
    failure = "";
    render();
    try {
      if (!pending) {
        let subscription: WebPushUpdate["subscription"] = null;
        if (turnOn) {
          if (!registration) return;
          const permission = await Notification.requestPermission();
          options.signal.throwIfAborted();
          if (permission !== "granted") return;
          const application_server_key = status.application_server_key;
          const key = keyBytes(application_server_key);
          if (!status.enabled || !usesKey(local, key)) {
            if (local) await local.unsubscribe();
            options.signal.throwIfAborted();
            local = await registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
          }
          if (!local) throw new Error("Browser subscription is missing");
          const { keys } = local.toJSON();
          if (!keys?.p256dh || !keys.auth) throw new Error("Browser subscription keys are missing");
          subscription = { endpoint: local.endpoint, p256dh: keys.p256dh, auth: keys.auth, application_server_key };
        }
        pending = { request_id: crypto.randomUUID(), expected_revision: status.revision, subscription };
      }
      options.signal.throwIfAborted();
      const command = pending;
      status = await api.update(command, options);
      pending = undefined;
      if (command.subscription === null && local) {
        options.signal.throwIfAborted();
        await local.unsubscribe();
        local = null;
      }
    } catch (error) {
      if (options.signal.aborted) return;
      const code = error instanceof ClientError ? error.status : undefined;
      if (code === 409) {
        pending = undefined;
        try { status = await api.status(options); } catch { /* Preserve the observed state. */ }
        failure = "Subscription changed. Check its current state.";
      } else {
        if (code !== undefined && code >= 400 && code < 500) pending = undefined;
        failure = pending ? "Couldn’t confirm the change." : "Couldn’t update notifications.";
      }
    } finally {
      changing = false;
      render();
    }
  }

  button.addEventListener("click", () => {
    if (!dialog.open) dialog.showModal();
    failure = "";
    void check();
    void prepare();
  }, options);
  enable.addEventListener("click", () => void change(true), options);
  disable.addEventListener("click", () => void change(false), options);
  close.addEventListener("click", () => dialog.close(), options);
  void check();
  return {
    check,
    async unsubscribe() {
      if (!supported) return;
      const worker = await navigator.serviceWorker.getRegistration("/");
      if (worker?.active?.scriptURL !== new URL("/notification-worker.js", location.href).href) return;
      const subscription = await worker.pushManager.getSubscription();
      await subscription?.unsubscribe();
    },
    destroy() { lifetime.abort(); dialog.remove(); button.hidden = true; },
  };
}
