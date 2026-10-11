// Everything about the wasm module except how its bytes are obtained.
//
// Shared by the page and by the Node CLI, so both drive exactly the same code.
// It imports nothing of its own, which is what lets it run in both.

export const encoder = new TextEncoder();
export const decoder = new TextDecoder();

/** 32 bytes from the platform CSPRNG, in hex. */
export function entropy() {
  const bytes = new Uint8Array(32);
  crypto.getRandomValues(bytes);
  return [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * The module is supposed to import nothing at all.
 *
 * Checked rather than assumed: a stray dependency would silently require a JS
 * shim, and the failure would show up as an opaque instantiation error.
 */
export function assertSelfContained(module) {
  const imports = WebAssembly.Module.imports(module);
  if (imports.length > 0) {
    throw new Error(
      `the wasm module has ${imports.length} imports (${imports[0].module}); ` +
        "it is supposed to be self-contained",
    );
  }
}

/** Wrap an instantiated module in the operations a taker needs. */
export function takerFrom(instance) {
  const { memory, alloc, dealloc, dispatch } = instance.exports;

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
    /** A fresh keypair. The secret is generated where this runs, and stays there. */
    keypair: () => call({ op: "keypair", entropy: entropy() }),
    htlcAddress: (params) => call({ op: "htlc_address", ...params }),
    findOutput: (params) => call({ op: "find_output", ...params }),
    checkJoin: (params) => call({ op: "check_join", ...params }),
    claim: (params) => call({ op: "claim", ...params }),
    refund: (params) => call({ op: "refund", ...params }),
  };
}
