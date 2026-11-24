import type { Dtos } from "../src/index.ts";
import type { RecoveryIdentity } from "./tab-drafts.ts";

type Head = Dtos["HistoryHead"];
type Mode = "revert" | "restore";
type ReviewActions = { fork?: (() => void) | undefined };
type Operation = Readonly<{ flow: Dtos["FlowAddress"]; requestId: string }>;
export type RecoveryAction = "recover" | "swap" | "return" | "blocked";
export type HistoryView =
  | { kind: "branches"; head: Head; branches: readonly Dtos["HistoryBranch"][]; loading: boolean; error?: string; next?: number | null;
      drafts: readonly (RecoveryIdentity & { action: RecoveryAction })[] }
  | ({ kind: "operation"; mode: Mode } & ReviewActions & (
    | { phase: "reviewing" }
    | { phase: "review_failed"; message: string }
    | { phase: "review"; review: Dtos["HistoryReviewResponse"] }
  ))
  | ({ kind: "operation"; mode: Mode; operation: Operation } & (
    | { phase: "preparing"; active: boolean }
    | { phase: "stopping" | "ready" | "installing" | "aborted" | "superseded" }
    | { phase: "unknown"; message: string; retry: boolean }
    | { phase: "install_failed"; message: string }
    | { phase: "pending"; label: string }
    | { phase: "applied"; head: Head; recoverable: readonly { id: string; label: string; action: RecoveryAction }[] }
  ));

export type HistoryAction =
  | { kind: "review_restore"; head: Head; branch: string }
  | { kind: "reload_branches"; head: Head }
  | { kind: "more_branches"; head: Head; offset: number }
  | { kind: "swap_saved"; head: Head; identity: RecoveryIdentity }
  | { kind: "begin"; request: Dtos["HistoryOperationRequest"] }
  | { kind: "stop" | "check" | "apply" | "abort" | "branches" | "retry_begin"; operation: Operation }
  | { kind: "recover"; operation: Operation; head: Head; input: string };
