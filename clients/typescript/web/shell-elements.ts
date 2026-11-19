import type { ActivityView, ExecutionActivity } from "./execution-activity.ts";

export type WorkbenchElements = {
  shell: HTMLElement;
  sidebarToggle: HTMLButtonElement;
  connectionState: HTMLElement;
  sidebar: HTMLElement;
  sessions: HTMLElement;
  search: HTMLInputElement;
  targetTitle: HTMLElement;
  tabs: HTMLElement;
  content: HTMLElement;
  composer: HTMLElement;
  status: HTMLElement;
  disconnectButton: HTMLButtonElement;
  newSessionButton: HTMLButtonElement;
  refreshButton: HTMLButtonElement;
  presentActivity: (view: ActivityView | undefined) => void;
  bindActivityActions: (actions: Pick<ExecutionActivity, "refresh" | "issue" | "retry">) => void;
};
