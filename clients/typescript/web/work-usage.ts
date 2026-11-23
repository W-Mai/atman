import type { Dtos } from "../src/index.ts";

export type UsageLocator = Dtos["ConversationUsageLocator"];
export type ReadUsage = (locator: UsageLocator, signal: AbortSignal) => Promise<Dtos["ReadConversationUsageResponse"]>;
export type UsageObservation = Readonly<{ key: string; locator: UsageLocator; read: ReadUsage }>;
