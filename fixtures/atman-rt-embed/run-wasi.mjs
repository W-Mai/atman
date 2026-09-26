import { readFileSync } from "node:fs";
import { WASI } from "node:wasi";

const wasi = new WASI({ version: "preview1", args: [], env: {}, preopens: {} });
const wasm = readFileSync(
  new URL("./target/wasm32-wasip1/debug/atman-rt-embed-fixture.wasm", import.meta.url),
);
const module = await WebAssembly.compile(wasm);
const instance = await WebAssembly.instantiate(module, {
  wasi_snapshot_preview1: wasi.wasiImport,
});
wasi.start(instance);
