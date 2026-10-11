// The Node loader: the same taker as the page, but the bytes come from disk.
//
// Used by `taker-cli.mjs`, and so by the shell scripts that drive a browser-side
// taker without a browser.

import { readFile } from "node:fs/promises";

import { assertSelfContained, takerFrom } from "./wasm.mjs";

export async function loadTaker(path) {
  const module = await WebAssembly.compile(await readFile(path));
  assertSelfContained(module);

  return takerFrom(await WebAssembly.instantiate(module, {}));
}
