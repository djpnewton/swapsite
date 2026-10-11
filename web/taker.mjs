// The page's loader for the taker module.
//
// Bytes come over HTTP from wherever the page was served, so this file has no
// Node imports and can be used directly by a browser.

import { assertSelfContained, takerFrom } from "./wasm.mjs";

export async function loadTaker(url = "/swapsite_wasm.wasm") {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`could not fetch the taker module: ${response.status}`);
  }

  // Compiled first so the import check runs before instantiation, which would
  // otherwise fail with something much less helpful.
  const module = await WebAssembly.compile(await response.arrayBuffer());
  assertSelfContained(module);

  return takerFrom(await WebAssembly.instantiate(module, {}));
}
