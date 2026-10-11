// The two things a taker without a node cannot do for itself: read the chain,
// and broadcast.
//
// Nothing here is trusted. The relay supplies bytes and carries transactions; the
// taker checks the deposit script itself and signs its own claim.

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
