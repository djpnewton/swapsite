// The browser-side taker.
//
// A thin wrapper over the wasm module, plus a client for the relay it uses to
// see the chain and to broadcast.
//
// Nothing here trusts the relay. It supplies bytes and carries transactions;
// the taker checks the deposit script itself and signs its own claim with a key
// the relay never sees.

import { readFile } from "node:fs/promises";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** 32 bytes from the platform CSPRNG, hex. In a page this is the browser's. */
export function entropy() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Load the wasm module and expose its dispatcher as plain functions.
 *
 * The module is expected to be self-contained: it has its own entropy supplied
 * by the page, so it should import nothing at all. That is checked here rather
 * than assumed, because a stray dependency would silently require a JS shim.
 */
export async function loadTaker(wasmPath) {
  const bytes = await readFile(wasmPath);
  const module = new WebAssembly.Module(bytes);

  const imports = WebAssembly.Module.imports(module);
  if (imports.length > 0) {
    throw new Error(
      `the wasm module has ${imports.length} imports (${imports[0].module}); ` +
        "it is supposed to be self-contained",
    );
  }

  const { exports } = new WebAssembly.Instance(module, {});
  const { memory, alloc, dealloc, dispatch } = exports;

  /** One JSON request in, one JSON response out, through linear memory. */
  function call(request) {
    const input = encoder.encode(JSON.stringify(request));
    const inputPtr = alloc(input.length);
    new Uint8Array(memory.buffer, inputPtr, input.length).set(input);

    const outputPtr = dispatch(inputPtr, input.length);
    dealloc(inputPtr, input.length);

    // Re-read the buffer each time: a call may have grown linear memory, which
    // detaches every view taken before it.
    const length = new DataView(memory.buffer).getUint32(outputPtr, true);
    const text = decoder.decode(
      new Uint8Array(memory.buffer, outputPtr + 4, length),
    );
    dealloc(outputPtr, length + 4);

    const response = JSON.parse(text);
    if (response.error) {
      throw new Error(response.error);
    }
    return response;
  }

  return {
    call,
    /** A fresh keypair. The secret is generated in the page and never leaves it. */
    keypair: () => call({ op: "keypair", entropy: entropy() }),
    htlcAddress: (params) => call({ op: "htlc_address", ...params }),
    findOutput: (params) => call({ op: "find_output", ...params }),
    checkJoin: (params) => call({ op: "check_join", ...params }),
    claim: (params) => call({ op: "claim", ...params }),
    refund: (params) => call({ op: "refund", ...params }),
  };
}

/** The two things a taker without a node cannot do for itself. */
export function relay(baseUrl) {
  return {
    async height() {
      const response = await fetch(`${baseUrl}/height`);
      if (!response.ok) {
        throw new Error(`relay /height: ${response.status}`);
      }
      return (await response.json()).height;
    },

    async transaction(txid) {
      const response = await fetch(`${baseUrl}/tx/${txid}`);
      if (response.status === 404) {
        return null;
      }
      if (!response.ok) {
        throw new Error(`relay /tx: ${response.status}`);
      }
      return response.json();
    },

    async broadcast(txHex) {
      // `text/plain` keeps this a CORS simple request, so a page needs no
      // preflight round trip.
      const response = await fetch(`${baseUrl}/tx`, {
        method: "POST",
        headers: { "Content-Type": "text/plain" },
        body: txHex,
      });
      const body = await response.json();
      if (!response.ok) {
        throw new Error(`relay broadcast: ${body.error ?? response.status}`);
      }
      return body.txid;
    },
  };
}
