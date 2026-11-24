import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "vite";

const root = fileURLToPath(new URL(".", import.meta.url));
const destination = resolve(root, "../../../crates/atman-daemon/assets/web");
const files = new Map([
  ["index.html", "index.html"],
  ["assets/atman.js", "atman.js"],
  ["assets/atman.css", "atman.css"],
  ["assets/ATMAN-LOGO.png", "ATMAN-LOGO.png"],
  ["manifest.webmanifest", "manifest.webmanifest"],
  ["notification-worker.js", "notification-worker.js"],
]);
const check = process.argv.includes("--check");
const staging = await mkdtemp(join(tmpdir(), "atman-web-"));
try {
  await build({
    configFile: resolve(root, "vite.config.ts"),
    publicDir: resolve(root, "paired-assets"),
    build: { outDir: staging, emptyOutDir: true },
  });
  const emitted = await readdir(staging, { recursive: true, withFileTypes: true });
  const paths = emitted
    .filter((entry) => !entry.isDirectory())
    .map((entry) =>
      relative(staging, join(entry.parentPath, entry.name)).split(sep).join("/"),
    );
  if (paths.length !== files.size || paths.some((path) => !files.has(path)))
    throw new Error(`Unexpected Web assets: ${paths.sort().join(", ")}`);

  const generated = await Promise.all(
    Array.from(files, async ([path, name]) => ({
      name,
      expected: await readFile(resolve(staging, path)),
    })),
  );
  if (!check) await mkdir(destination, { recursive: true });
  for (const { name, expected } of generated) {
    const path = resolve(destination, name);
    if (!check) {
      await writeFile(path, expected);
    } else if (!(await readFile(path)).equals(expected)) {
      throw new Error(`Generated Web asset differs: ${name}`);
    }
  }
} finally {
  await rm(staging, { recursive: true, force: true });
}
console.log(`Web assets ${check ? "verified" : "built"}`);
