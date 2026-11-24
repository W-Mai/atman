import { fileURLToPath } from "node:url";
import { defineConfig, mergeConfig } from "vite";
import web from "../web/vite.config.ts";

export default defineConfig(mergeConfig(web, {
  root: fileURLToPath(new URL(".", import.meta.url)),
  base: "./",
}));
