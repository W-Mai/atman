export type FailureKind =
  | "request"
  | "capability"
  | "transport"
  | "timeout"
  | "aborted"
  | "http"
  | "protocol"
  | "source"
  | "limit";

export type CommandOutcome = "not_sent" | "unknown" | "not_applicable";

/** Transport failure does not establish whether a command was accepted. */
export class ClientError extends Error {
  override readonly name = "ClientError";

  constructor(
    readonly kind: FailureKind,
    message: string,
    readonly outcome: CommandOutcome = "not_applicable",
    /** HTTP status when the error comes from an actual response. */
    readonly status?: number,
  ) {
    super(message);
  }
}

/** The original JSON-RPC error, without inferring that an operation had no effects. */
export class RemoteError extends Error {
  override readonly name = "RemoteError";

  constructor(
    readonly code: number,
    message: string,
    readonly data: unknown,
  ) {
    super(message);
  }
}

export function cancelled(): DOMException {
  return new DOMException("Observation was cancelled", "AbortError");
}
