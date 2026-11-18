import type { Dtos, FlowClient } from "../src/index.ts";

function effectiveEffort(
  selection: Dtos["ComposerReasoning"],
): string | undefined {
  switch (selection.type) {
    case "provider_default":
      return undefined;
    case "disabled":
      return "Off";
    case "budget_tokens":
      return `${selection.tokens.toLocaleString()} tokens`;
    case "auto":
      return selection.execution_mode
        ? `Auto · ${selection.execution_mode}`
        : "Auto";
    case "effort":
      return selection.execution_mode
        ? `${selection.effort} · ${selection.execution_mode}`
        : selection.effort;
  }
}

export function composerSettings(
  flow: FlowClient,
  signal: AbortSignal,
): HTMLElement {
  const settings = document.createElement("span");
  settings.className = "composer-settings message-note";
  settings.hidden = true;
  settings.setAttribute("role", "group");
  settings.setAttribute("aria-label", "Composer settings");

  function append(label: string, title: string): void {
    if (settings.hasChildNodes()) settings.append(" · ");
    const item = document.createElement("span");
    item.textContent = label;
    item.title = title;
    settings.append(item);
  }

  // One read belongs to this selection. Closing it also retires any late response.
  void flow
    .composer({}, { signal })
    .then(({ result }) => {
      if (signal.aborted) return;
      const { model, trust } = result;
      if (model.status === "ready") {
        append(model.model, `${model.model} · ${model.provider}`);
        if (model.reasoning.available && model.reasoning.effective != null) {
          const effort = effectiveEffort(model.reasoning.effective);
          if (effort !== undefined) append(effort, `Reasoning: ${effort}`);
        }
      }
      const title =
        trust.escalation == null
          ? trust.description
          : `${trust.description} · ${trust.escalation.name}`;
      append(`${trust.icon} ${trust.name}`, title);
      settings.hidden = false;
    })
    .catch(() => {
      // Unloaded targets and older hosts have no settings to display.
    });
  return settings;
}
