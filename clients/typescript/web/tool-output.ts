import type { Dtos } from "../src/index.ts";

export type ArtifactTarget = Readonly<{
  key: string;
  locator: Dtos["ConversationArtifactLocator"];
  bytes: number;
}>;
export type ArtifactTargets = WeakMap<Dtos["ConversationPart"], ArtifactTarget>;
type VerificationProgress = Extract<Dtos["ReadConversationArtifactResponse"], { state: "verifying" }>;
export type ReadArtifact = (
  target: ArtifactTarget, offset: number, signal: AbortSignal, progress: (value: VerificationProgress) => void,
) => Promise<Exclude<Dtos["ReadConversationArtifactResponse"], VerificationProgress>>;

export function recordArtifacts(targets: ArtifactTargets, page: Dtos["ConversationPage"]): void {
  for (const item of page.items) {
    if (item.body.kind !== "message" || item.body.role !== "tool") continue;
    for (const [index, part] of item.body.parts.entries()) {
      const ordinal = item.source_parts?.[index];
      if (part.kind !== "tool_result" || part.artifact == null || ordinal == null ||
          !Number.isSafeInteger(ordinal) || ordinal < 0 || ordinal > 0xffffffff) continue;
      const { source, head } = page;
      const { id, bytes } = part.artifact;
      targets.set(part, {
        key: JSON.stringify([source.flow.session_id, source.flow.instance_id, source.cursor.journal_id,
          source.branch, head.version, item.id.commit, item.id.fact, item.id.item, item.id.message, ordinal, id, bytes]),
        locator: { source, head, item: item.id, part: ordinal, id },
        bytes,
      });
    }
  }
}
