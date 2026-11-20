import type { FlowOverview, OverviewView } from "./flow-overview.ts";
import type { ApprovalActivity, ApprovalView } from "./approval-activity.ts";
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
  showConversationTarget: (target: HTMLElement) => void;
  presentOverview: (view: OverviewView | undefined) => void;
  bindOverviewActions: (actions: Pick<FlowOverview, "refresh" | "visible">) => void;
  presentActivity: (view: ActivityView | undefined) => void;
  presentApprovals: (view: ApprovalView | undefined) => void;
  bindApprovalActions: (actions: Pick<ApprovalActivity, "refresh" | "review" | "dismiss" | "issue">) => void;
  bindActivityActions: (actions: Pick<ExecutionActivity, "refresh" | "issue" | "retry">) => void;
};
