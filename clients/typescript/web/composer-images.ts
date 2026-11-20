import type { Dtos } from "../src/index.ts";
import type { DraftAttachments } from "./tab-drafts.ts";

export const imageMimeTypes = "image/png,image/jpeg,image/gif,image/webp";
export const maxImages = 16;
// /rpc extracts a String with a 2 MiB body limit. Leave room for the envelope and history head.
const userContentLimit = 2 * 1024 * 1024 - 4 * 1024;
export const imageSizeMessage = "Message and attachments exceed the Web upload limit (2 MiB encoded).";
const encoder = new TextEncoder();

export function fitsImageSubmission(text: string, attachments: DraftAttachments): boolean {
  return encoder.encode(JSON.stringify({ text, ...attachments })).length <= userContentLimit;
}

export function clipboardImages(clipboard: DataTransfer): File[] {
  const items = Array.from(clipboard.items).flatMap((item) => {
    const file = item.kind === "file" && item.type.startsWith("image/") ? item.getAsFile() : null;
    return file ? [file] : [];
  });
  return items.length ? items : Array.from(clipboard.files).filter((file) => file.type.startsWith("image/"));
}

function startsWith(bytes: Uint8Array, prefix: readonly number[], offset = 0): boolean {
  return prefix.every((byte, index) => bytes[index + offset] === byte);
}

function format(bytes: Uint8Array): string | undefined {
  if (startsWith(bytes, [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])) return "png";
  if (startsWith(bytes, [0xff, 0xd8, 0xff])) return "jpg";
  if (startsWith(bytes, [0x47, 0x49, 0x46, 0x38]) && (bytes[4] === 0x37 || bytes[4] === 0x39) && bytes[5] === 0x61) return "gif";
  if (startsWith(bytes, [0x52, 0x49, 0x46, 0x46]) && startsWith(bytes, [0x57, 0x45, 0x42, 0x50], 8)) return "webp";
}

function base64(bytes: Uint8Array): string {
  let encoded = "";
  // Every complete chunk ends on a base64 group boundary.
  const chunk = 24 * 1024;
  for (let start = 0; start < bytes.length; start += chunk) {
    encoded += btoa(String.fromCharCode(...bytes.subarray(start, start + chunk)));
  }
  return encoded;
}

export async function readComposerImages(
  files: readonly File[], existing: readonly Dtos["InlineImage"][], stillCurrent: () => boolean,
): Promise<Dtos["InlineImage"][]> {
  if (files.length + existing.length > maxImages) throw new Error(`Attach up to ${maxImages} images.`);
  let remaining = userContentLimit - existing.reduce((size, image) => size + image.data_base64.length, 0);
  for (const file of files) {
    remaining -= Math.ceil(file.size / 3) * 4;
    if (remaining < 0) throw new Error(imageSizeMessage);
  }
  const images: Dtos["InlineImage"][] = [];
  for (const file of files) {
    if (!stillCurrent()) return [];
    const bytes = new Uint8Array(await file.arrayBuffer());
    if (!stillCurrent()) return [];
    const extension = format(bytes);
    if (!extension) throw new Error("Choose a PNG, JPEG, GIF, or WebP image.");
    const name = Array.from(file.name.replace(/[\u0000-\u001f\u007f]/g, "")).slice(0, 128).join("");
    images.push({ name: name || `Image.${extension}`, data_base64: base64(bytes) });
  }
  return images;
}
