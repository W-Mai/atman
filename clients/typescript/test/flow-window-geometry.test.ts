import { expect, test } from "bun:test";
import {
  cascadePanel,
  clampPanel,
  consoleUnderPanel,
  panelCanvas,
  panelGap,
  PanelSpring,
} from "../web/flow-window-geometry.ts";

test("panel geometry keeps the Console beyond its bottom edge and inside the workspace", () => {
  const workspace = { width: 920, height: 610 };
  const console = { width: 680, height: 124 };
  const bounds = panelCanvas(workspace, console.height);
  const first = cascadePanel(0, bounds);
  const second = cascadePanel(1, bounds);
  expect(second.x).toBeGreaterThan(first.x);
  expect(second.y).toBeGreaterThan(first.y);
  for (const panel of [first, second, clampPanel({ ...second, x: 1000, y: 900 }, bounds)]) {
    const dock = consoleUnderPanel(panel, workspace, console);
    expect(dock.x + dock.width / 2).toBe(panel.x + panel.width / 2);
    expect(dock.y).toBe(panel.y + panel.height + panelGap);
    expect(dock.y + dock.height).toBeLessThanOrEqual(workspace.height);
  }
});

test("Console spring projects position and size at the same frame rate and settles exactly", () => {
  const initial = { x: 25, y: 450, width: 680, height: 110 };
  const target = { x: 65, y: 250, width: 420, height: 136 };
  const direct = new PanelSpring(initial);
  const stepped = new PanelSpring(initial);
  const middle = direct.advance(target, 0.06);
  const first = stepped.advance(target, 0.02);
  expect(first.x).toBeGreaterThan(initial.x);
  expect(first.y).toBeLessThan(initial.y);
  expect(first.width).toBeLessThan(initial.width);
  expect(first.height).toBeGreaterThan(initial.height);
  const equivalent = stepped.advance(target, 0.04);
  for (const axis of ["x", "y", "width", "height"] as const) {
    expect(equivalent[axis]).toBeCloseTo(middle[axis], 10);
  }
  expect(direct.advance(target, 1)).toBe(target);
});
