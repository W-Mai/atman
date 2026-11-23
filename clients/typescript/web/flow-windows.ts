import { cascadePanel, clampPanel, consoleUnderPanel, panelCanvas, panelGap, PanelSpring } from "./flow-window-geometry.ts";
import type { PanelRect } from "./flow-window-geometry.ts";

type Panel = {
  key: string;
  node: HTMLElement;
  content: HTMLElement;
  preferred: PanelRect;
  displayed: PanelRect;
  retire: () => void;
};

export function flowWindows(main: HTMLElement, content: HTMLElement, status: HTMLElement, composer: HTMLElement,
  select: (key?: string) => void) {
  const layer = document.createElement("div");
  layer.className = "flow-window-layer";
  main.append(layer);
  const narrow = matchMedia("(width < 64rem)");
  const reduce = matchMedia("(prefers-reduced-motion: reduce)");
  const lifetime = new AbortController();
  const remembered = new Map<string, PanelRect>();
  const panels = new Map<string, Panel>();
  let active: Panel | undefined;
  let ordinal = 0;
  let z = 0;
  let frame = 0;
  let last = 0;
  let motion: PanelSpring | undefined;
  let dragging: Panel | undefined;

  function dimensions() {
    return { width: layer.clientWidth, height: layer.clientHeight };
  }

  function bounds() {
    return panelCanvas(dimensions(), composer.offsetHeight);
  }

  function position(node: HTMLElement, rect: PanelRect): void {
    node.style.setProperty("--panel-x", `${rect.x}px`);
    node.style.setProperty("--panel-y", `${rect.y}px`);
    node.style.setProperty("--panel-width", `${rect.width}px`);
    node.style.setProperty("--panel-height", `${rect.height}px`);
  }

  function apply(now: number): void {
    frame = 0;
    const desktop = !narrow.matches;
    content.inert = status.inert = active !== undefined && !desktop;
    main.style.setProperty("--panel-console-height", `${composer.offsetHeight}px`);
    for (const panel of panels.values()) {
      panel.node.inert = !desktop && panel !== active;
      panel.node.classList.toggle("active", panel === active);
      if (desktop) {
        panel.displayed = clampPanel(panel.preferred, bounds());
        position(panel.node, panel.displayed);
      }
    }
    if (narrow.matches || (!active && !motion)) {
      composer.classList.remove("flow-console-positioned");
      main.classList.remove("flow-console-motion");
      motion = undefined;
      return;
    }
    const workspace = dimensions();
    const measured = composer.getBoundingClientRect();
    const height = composer.offsetHeight;
    const origin = layer.getBoundingClientRect();
    motion ??= new PanelSpring({ x: 0, y: measured.top - origin.top, width: measured.width, height });
    // Animate the composer slot; its inner form retains its existing maximum width.
    const target = active
      ? consoleUnderPanel(active.displayed, workspace, { width: workspace.width, height })
      : { x: 0, y: workspace.height - height, width: workspace.width, height };
    const displayed = reduce.matches || dragging ? motion.snap(target) : motion.advance(target, (now - last) / 1000);
    last = now;
    position(composer, displayed);
    composer.style.setProperty("--panel-scale-y", String(height ? displayed.height / height : 1));
    composer.classList.add("flow-console-positioned");
    main.classList.add("flow-console-motion");
    if (displayed !== target) queue();
    else if (!active) {
      composer.classList.remove("flow-console-positioned");
      main.classList.remove("flow-console-motion");
      motion = undefined;
    }
  }

  function queue(): void {
    if (!frame && !lifetime.signal.aborted) frame = requestAnimationFrame(apply);
  }

  function focus(panel?: Panel): void {
    if (panel === active) return;
    active = panel;
    if (panel) panel.node.style.zIndex = String(++z);
    for (const candidate of panels.values()) candidate.node.classList.toggle("active", candidate === panel);
    last = performance.now();
    select(panel?.key);
    if (narrow.matches) {
      for (const candidate of panels.values()) candidate.node.inert = candidate !== panel;
      content.inert = status.inert = panel !== undefined;
    }
    queue();
  }

  function move(panel: Panel, resize: boolean, dx: number, dy: number, origin: PanelRect): void {
    const canvas = bounds();
    panel.preferred = clampPanel(resize ? {
      ...origin,
      width: Math.min(origin.width + dx, canvas.width - origin.x - panelGap),
      height: Math.min(origin.height + dy, canvas.height - origin.y - panelGap),
    } : { ...origin, x: origin.x + dx, y: origin.y + dy }, canvas);
    queue();
  }

  function pointer(panel: Panel, handle: HTMLElement, resizing: boolean): void {
    handle.addEventListener("pointerdown", (event) => {
      if (narrow.matches || event.button !== 0 || (event.target as Element).closest("button") !== (resizing ? handle : null)) return;
      focus(panel);
      const start = panel.displayed;
      const { clientX: x, clientY: y, pointerId: id } = event;
      handle.setPointerCapture(id);
      dragging = panel;
      function update(next: PointerEvent) {
        if (next.pointerId === id) move(panel, resizing, next.clientX - x, next.clientY - y, start);
      }
      handle.addEventListener("pointermove", update);
      handle.addEventListener("lostpointercapture", () => {
        handle.removeEventListener("pointermove", update);
        if (dragging === panel) dragging = undefined;
      }, { once: true });
    });
  }

  function close(panel: Panel): void {
    panels.delete(panel.key);
    remembered.delete(panel.key);
    remembered.set(panel.key, panel.preferred);
    if (remembered.size > 24) remembered.delete(remembered.keys().next().value!);
    const wasActive = active === panel;
    panel.retire();
    panel.node.remove();
    if (wasActive) {
      const next = [...panels.values()].sort((a, b) => Number(b.node.style.zIndex) - Number(a.node.style.zIndex))[0];
      focus(next);
      (next?.content ?? content).focus({ preventScroll: true });
    }
    queue();
  }

  const resize = new ResizeObserver(queue);
  resize.observe(layer);
  resize.observe(composer);
  narrow.addEventListener("change", queue, { signal: lifetime.signal });
  reduce.addEventListener("change", queue, { signal: lifetime.signal });
  content.addEventListener("pointerdown", () => focus(), { signal: lifetime.signal });
  content.addEventListener("focus", () => focus(), { signal: lifetime.signal });
  main.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || event.defaultPrevented || event.isComposing || !active ||
        !(event.target instanceof Node) || (!layer.contains(event.target) && !composer.contains(event.target))) return;
    event.preventDefault();
    close(active);
  }, { signal: lifetime.signal });

  return {
    close(key: string): void {
      const panel = panels.get(key);
      if (panel) close(panel);
    },
    open(key: string, label: string, launch: (pane: HTMLElement, status: HTMLElement) => () => void): boolean {
      const old = panels.get(key);
      if (old) {
        focus(old);
        old.content.focus({ preventScroll: true });
        return true;
      }
      if (panels.size >= 8) return false;
      const node = document.createElement("section");
      node.className = "flow-window";
      node.setAttribute("aria-label", label);
      const header = document.createElement("header");
      header.tabIndex = 0;
      header.setAttribute("aria-label", `Move ${label}`);
      const heading = document.createElement("span");
      heading.textContent = label;
      const dismiss = document.createElement("button");
      dismiss.type = "button";
      dismiss.textContent = "×";
      dismiss.title = "Close panel";
      dismiss.setAttribute("aria-label", dismiss.title);
      header.append(heading, dismiss);
      const pane = document.createElement("div");
      pane.className = "content flow-window-content";
      pane.tabIndex = 0;
      pane.setAttribute("role", "region");
      pane.setAttribute("aria-label", `${label} transcript`);
      const feedback = document.createElement("div");
      feedback.className = "flow-window-status";
      feedback.setAttribute("role", "status");
      const grip = document.createElement("button");
      grip.type = "button";
      grip.textContent = "◢";
      grip.className = "flow-window-resize";
      grip.setAttribute("aria-label", "Resize panel with arrow keys");
      node.append(header, pane, feedback, grip);
      layer.append(node);
      const preferred = remembered.get(key) ?? cascadePanel(ordinal++, bounds());
      const panel = { key, node, content: pane, preferred, displayed: preferred, retire: launch(pane, feedback) };
      panels.set(key, panel);
      node.addEventListener("pointerdown", () => focus(panel));
      node.addEventListener("focusin", () => focus(panel));
      dismiss.onclick = () => close(panel);
      pointer(panel, header, false);
      pointer(panel, grip, true);
      for (const handle of [header, grip]) handle.addEventListener("keydown", (event) => {
        if (narrow.matches || event.target !== handle) return;
        const dx = event.key === "ArrowLeft" ? -16 : event.key === "ArrowRight" ? 16 : 0;
        const dy = event.key === "ArrowUp" ? -16 : event.key === "ArrowDown" ? 16 : 0;
        if (!dx && !dy) return;
        event.preventDefault();
        move(panel, handle === grip, dx, dy, panel.displayed);
      });
      focus(panel);
      pane.focus({ preventScroll: true });
      return true;
    },
    main(): void { focus(); },
    focus(key: string): void {
      const panel = panels.get(key);
      if (panel) focus(panel);
    },
    activeContent(): HTMLElement | undefined { return active?.content; },
    reset(): void {
      for (const panel of panels.values()) {
        panel.retire();
        panel.node.remove();
      }
      const hadActive = active !== undefined;
      panels.clear();
      remembered.clear();
      active = undefined;
      ordinal = z = 0;
      if (hadActive) select();
      queue();
    },
    destroy(): void {
      this.reset();
      lifetime.abort();
      resize.disconnect();
      cancelAnimationFrame(frame);
      layer.remove();
      composer.classList.remove("flow-console-positioned");
      main.classList.remove("flow-console-motion");
    },
  };
}
