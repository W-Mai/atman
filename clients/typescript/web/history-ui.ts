import type { Dtos } from "../src/index.ts";

type Head = Dtos["HistoryHead"];
type Mode = "revert" | "restore";
type Operation = Readonly<{ flow: Dtos["FlowAddress"]; requestId: string }>;
export type HistoryView =
  | { kind: "branches"; head: Head; branches: readonly Dtos["HistoryBranch"][]; loading: boolean; error?: string; next?: number | null }
  | { kind: "operation"; mode: Mode; phase: "reviewing" }
  | { kind: "operation"; mode: Mode; phase: "review_failed"; message: string }
  | { kind: "operation"; mode: Mode; phase: "review"; review: Dtos["HistoryReviewResponse"] }
  | ({ kind: "operation"; mode: Mode; operation: Operation } & (
    | { phase: "preparing"; active: boolean }
    | { phase: "stopping" | "ready" | "installing" | "aborted" | "superseded" }
    | { phase: "unknown"; message: string; retry: boolean }
    | { phase: "install_failed"; message: string }
    | { phase: "pending"; label: string }
    | { phase: "applied"; head: Head; recoverable: readonly { id: string; label: string }[]; draftOccupied: boolean }
  ));

export type HistoryAction =
  | { kind: "review_restore"; head: Head; branch: string }
  | { kind: "reload_branches"; head: Head }
  | { kind: "more_branches"; head: Head; offset: number }
  | { kind: "begin"; request: Dtos["HistoryOperationRequest"] }
  | { kind: "stop" | "check" | "apply" | "abort" | "branches" | "retry_begin"; operation: Operation }
  | { kind: "recover"; operation: Operation; head: Head; input: string };
