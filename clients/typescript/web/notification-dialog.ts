export function notificationDialog(button: HTMLButtonElement) {
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
  const action = (label: string) => {
    const control = document.createElement("button");
    control.type = "button";
    control.textContent = label;
    return control;
  };
  const enable = action("Turn on");
  enable.className = "primary";
  const disable = action("Turn off");
  const close = action("Close");
  actions.append(close, disable, enable);
  dialog.append(title, message, actions);
  document.body.append(dialog);
  button.setAttribute("aria-label", "Notifications");
  button.title = "Notifications";
  return { dialog, message, actions, enable, disable, close };
}
