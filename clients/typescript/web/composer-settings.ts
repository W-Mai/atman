import type { Dtos } from "../src/index.ts";

type Reasoning = Dtos["ComposerReasoning"];
const effortRanks = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra", "persistent"];

function label(selection: Reasoning): string {
  switch (selection.type) {
    case "provider_default":
      return "Default";
    case "disabled":
      return "Off";
    case "budget_tokens":
      return selection.tokens.toLocaleString() + " tokens";
    case "auto":
      return selection.execution_mode ? "Auto · " + selection.execution_mode : "Auto";
    case "effort":
      return selection.execution_mode ? selection.effort + " · " + selection.execution_mode : selection.effort;
  }
}

export function projectReasoning(
  snapshot: Dtos["ComposerReasoningState"],
): { title: string; filled: number } | undefined {
  if (!snapshot.available) return undefined;
  const current = snapshot.effective;
  const displayed = snapshot.requested ?? snapshot.configured;
  let title = "Reasoning · " + label(displayed);
  if (snapshot.effective && label(displayed) !== label(snapshot.effective)) {
    title += " · effective " + label(snapshot.effective);
  }
  if (snapshot.error) title += " · " + snapshot.error;

  let filled = 0;
  if (current?.type === "effort" || current?.type === "budget_tokens") {
    const order = (choice: Reasoning): number =>
      current.type === "effort" && choice.type === "effort" &&
      (current.execution_mode ?? null) === (choice.execution_mode ?? null)
        ? effortRanks.indexOf(choice.effort)
        : current.type === "budget_tokens" && choice.type === "budget_tokens" ? choice.tokens : -1;
    const points = [...new Set(snapshot.choices.map(order).filter((value) => value >= 0))]
      .sort((left, right) => left - right);
    const index = points.indexOf(order(current));
    if (index >= 0) {
      filled = index === points.length - 1
        ? 8
        : Math.min(7, Math.max(1, Math.round(8 * (index + 1) / points.length)));
    }
  }
  return { title, filled };
}
