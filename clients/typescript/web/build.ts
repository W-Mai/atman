import { mkdir, readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";

const destination = resolve(
  import.meta.dir,
  "../../../crates/atman-daemon/assets/web",
);
const build = await Bun.build({
  entrypoints: [resolve(import.meta.dir, "app.ts")],
  target: "browser",
  format: "esm",
  minify: true,
});
if (!build.success) throw new AggregateError(build.logs, "Web build failed");
const output = build.outputs[0];
if (output === undefined) throw new Error("Web bundle is missing");
const staticRoot = resolve(import.meta.dir, "../../../static");
const landing = await readFile(
  resolve(staticRoot, "atman-landing.css"),
  "utf8",
);
const palette = landing.match(/^:root\s*\{[\s\S]*?^\}/m)?.[0];
if (!palette) throw new Error("Shared atman palette is missing");
const styles = await readFile(resolve(import.meta.dir, "app.css"), "utf8");
const files = new Map<string, Buffer>([
  ["atman.js", Buffer.from(await output.arrayBuffer())],
  ["atman.css", Buffer.from(`${palette}\n\n${styles}`)],
  ["index.html", await readFile(resolve(import.meta.dir, "index.html"))],
  ["ATMAN-LOGO.png", await readFile(resolve(staticRoot, "ATMAN-LOGO.png"))],
]);
if (process.argv.includes("--check")) {
  for (const [name, expected] of files) {
    const actual = await readFile(resolve(destination, name));
    if (!actual.equals(expected))
      throw new Error(`Generated Web asset differs: ${name}`);
  }
} else {
  await mkdir(destination, { recursive: true });
  for (const [name, value] of files)
    await writeFile(resolve(destination, name), value);
}
console.log(
  `Web assets ${process.argv.includes("--check") ? "verified" : "built"}`,
);
