export type PanelRect = Readonly<{ x: number; y: number; width: number; height: number }>;
export type PanelBounds = Readonly<{ width: number; height: number }>;

export const panelGap = 12;
const inset = 12;
const minimumWidth = 300;
const minimumHeight = 160;

function clamp(value: number, low: number, high: number): number {
  return Math.min(Math.max(value, low), high);
}

export function panelCanvas(workspace: PanelBounds, consoleHeight: number): PanelBounds {
  return { width: workspace.width, height: Math.max(0, workspace.height - consoleHeight - panelGap) };
}

export function clampPanel(rect: PanelRect, bounds: PanelBounds): PanelRect {
  const edgeX = Math.min(inset, bounds.width / 2);
  const edgeY = Math.min(inset, bounds.height / 2);
  const maxWidth = bounds.width - 2 * edgeX;
  const maxHeight = bounds.height - 2 * edgeY;
  const width = clamp(rect.width, Math.min(maxWidth, minimumWidth), maxWidth);
  const height = clamp(rect.height, Math.min(maxHeight, minimumHeight), maxHeight);
  return {
    x: clamp(rect.x, edgeX, bounds.width - width - edgeX),
    y: clamp(rect.y, edgeY, bounds.height - height - edgeY),
    width,
    height,
  };
}

export function cascadePanel(ordinal: number, bounds: PanelBounds): PanelRect {
  const step = (ordinal % 7) * 24;
  return clampPanel({
    x: inset + step,
    y: inset + step,
    width: Math.round(bounds.width * 0.7),
    height: Math.round(bounds.height * 0.72),
  }, bounds);
}

export function consoleUnderPanel(
  panel: PanelRect,
  workspace: PanelBounds,
  desired: PanelBounds,
): PanelRect {
  const width = Math.min(desired.width, panel.width, workspace.width);
  const height = Math.min(desired.height, workspace.height);
  return {
    x: clamp(panel.x + (panel.width - width) / 2, 0, workspace.width - width),
    y: clamp(panel.y + panel.height + panelGap, 0, workspace.height - height),
    width,
    height,
  };
}

const axes = ["x", "y", "width", "height"] as const;
const zero: PanelRect = { x: 0, y: 0, width: 0, height: 0 };

// Advance the exact critically damped solution, independent of frame spacing.
export class PanelSpring {
  #velocity = zero;
  constructor(public rect: PanelRect, private readonly frequency = 26) {}

  snap(goal: PanelRect): PanelRect {
    this.#velocity = zero;
    return this.rect = goal;
  }

  advance(goal: PanelRect, seconds: number): PanelRect {
    if (seconds <= 0) return this.rect;
    const speed = this.frequency;
    const decay = Math.exp(-seconds * speed);
    const rect = { ...zero };
    const velocity = { ...zero };
    for (const axis of axes) {
      const offset = this.rect[axis] - goal[axis];
      const slope = this.#velocity[axis] + speed * offset;
      rect[axis] = goal[axis] + (offset + slope * seconds) * decay;
      velocity[axis] = (this.#velocity[axis] - slope * seconds * speed) * decay;
    }
    if (axes.every((axis) => Math.abs(rect[axis] - goal[axis]) < 0.25 && Math.abs(velocity[axis]) < 1)) {
      return this.snap(goal);
    }
    this.#velocity = velocity;
    return this.rect = rect;
  }
}
