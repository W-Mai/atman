import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig, normalizePath } from "vite";

const root = fileURLToPath(new URL(".", import.meta.url));
const devOrigin = process.env.ATMAN_WEB_DEV_ORIGIN;
const staticRoot = resolve(root, "../../../static");
const landingStyles = resolve(staticRoot, "atman-landing.css");
const appStyles = normalizePath(resolve(root, "app.css"));

export default defineConfig({
  root,
  publicDir: false,
  plugins: [
    svelte(),
    {
      name: "atman-shared-palette",
      enforce: "pre",
      async transform(code, id) {
        if (id !== appStyles) return;
        this.addWatchFile(landingStyles);
        const landing = await readFile(landingStyles, "utf8");
        const palette = landing.match(/^:root\s*\{[\s\S]*?^\}/m)?.[0];
        if (!palette) throw new Error("Shared atman palette is missing");
        return `${palette}\n\n${code}`;
      },
    },
  ],
  server: {
    host: "127.0.0.1",
    fs: { allow: [resolve(root, ".."), staticRoot] },
    ...(devOrigin
      ? {
          proxy: Object.fromEntries(
            ["/rpc", "/events", "/openapi.json"].map((route) => [
              route,
              devOrigin,
            ]),
          ),
        }
      : {}),
  },
  build: {
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    modulePreload: false,
    rolldownOptions: {
      output: {
        codeSplitting: false,
        entryFileNames: "assets/atman.js",
        assetFileNames: ({ names }) =>
          names.some((name) => name.endsWith(".css"))
            ? "assets/atman.css"
            : "assets/[name][extname]",
      },
    },
  },
});
