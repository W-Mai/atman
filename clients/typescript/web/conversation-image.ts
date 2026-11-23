import type { Dtos } from "../src/index.ts";

export type ImageTarget = Readonly<{
  key: string;
  locator: Dtos["ConversationImageLocator"];
}>;
export type ImageTargets = WeakMap<Dtos["ConversationPart"], ImageTarget>;
export type ReadImage = (target: ImageTarget, signal: AbortSignal) =>
  Promise<Dtos["ReadConversationImageResponse"]>;

export function recordImages(targets: ImageTargets, page: Dtos["ConversationPage"]): void {
  const { source, head } = page;
  for (const item of page.items) {
    if (item.body.kind !== "message" || item.body.role === "system") continue;
    for (const [index, part] of item.body.parts.entries()) {
      if (part.kind !== "image") continue;
      const ordinal = item.source_parts?.[index];
      if (ordinal == null || !Number.isSafeInteger(ordinal) || ordinal < 0 || ordinal > 0xffffffff) continue;
      targets.set(part, {
        key: JSON.stringify([source.flow.session_id, source.flow.instance_id, source.cursor.journal_id,
          head.branch, head.version, item.id.commit, item.id.fact, item.id.item, item.id.message, ordinal]),
        locator: { source, head, item: item.id, part: ordinal },
      });
    }
  }
}

export function imageBlob(result: Extract<Dtos["ReadConversationImageResponse"], { state: "ready" }>): Blob {
  const decoded = atob(result.data_base64);
  if (decoded.length === 0 || decoded.length > 4 * 1024 * 1024) throw new Error("Invalid image size");
  const bytes = new Uint8Array(decoded.length);
  for (let index = 0; index < decoded.length; index++) bytes[index] = decoded.charCodeAt(index);
  return new Blob([bytes], { type: result.media_type });
}
