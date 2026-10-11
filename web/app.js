// The page side of a swap.
//
// Every piece of cryptography happens in the wasm module. This file is the
// plumbing around it: ask the coordinator for a quote, ask it for a swap, check
// the deposit against the chain through the relay, pay, then claim.
//
// What lives here that matters:
//   - the claim key, generated in this page and never sent anywhere;
//   - the preimage, once the payment reveals it.
//
// Both are kept in localStorage so that reloading the page does not lose a swap.
// Losing the preimage after the maker settles means losing the swap to the
// maker's timeout, so it is worth persisting. A real product would protect these
// with a passphrase or a hardware signer.

import { relay } from "/relay.mjs";
import { loadTaker } from "/taker.mjs";

const taker = await loadTaker("/swapsite_wasm.wasm");
const config = await (await fetch("/config")).json();
const chain = relay(config.relay_url);

const STORAGE = "swapsite.session";
const session = JSON.parse(localStorage.getItem(STORAGE) ?? "{}");
session.key ??= taker.keypair();
session.swaps ??= {};
const persist = () => localStorage.setItem(STORAGE, JSON.stringify(session));

const $ = (id) => document.getElementById(id);
const sats = (n) => `${Number(n).toLocaleString()} sat`;
const short = (text, head = 10, tail = 6) =>
  text.length <= head + tail ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;

async function api(path, body) {
  const options =
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: { "Content-Type": "text/plain" },
          body: typeof body === "string" ? body : JSON.stringify(body),
        };

  const response = await fetch(path, options);
  const payload = await response.json();
  if (!response.ok) {
    throw new Error(payload.error ?? `HTTP ${response.status}`);
  }
  return payload;
}

function update(id, patch) {
  session.swaps[id] = { ...session.swaps[id], ...patch };
  persist();
  render();
}

// --- actions ---------------------------------------------------------------

let quote = null;

async function askQuote() {
  const amount = Number($("amount").value);
  quote = await api(`/quote?amount=${amount}`);
  $("quote").textContent =
    `You pay ${sats(quote.invoice_sat)} over Lightning and receive ` +
    `${sats(quote.amount_sat)} on chain. Maker's fee: ${sats(quote.fee_sat)}.`;
  return quote;
}

async function create() {
  $("create").disabled = true;
  $("quote").textContent = "Getting a quote…";
  try {
    const { amount_sat } = await askQuote();
    const swap = await api("/swaps", {
      amount: amount_sat,
      claim_pubkey: session.key.pubkey,
    });
    update(swap.id, { swap });
  } catch (error) {
    $("quote").textContent = `Could not start a swap: ${error.message}`;
  } finally {
    $("create").disabled = false;
  }
}

async function refresh() {
  for (const id of Object.keys(session.swaps)) {
    try {
      const swap = await api(`/swaps/${id}`);
      if (JSON.stringify(swap) !== JSON.stringify(session.swaps[id].swap)) {
        update(id, { swap });
      }
    } catch {
      // The coordinator being down is not fatal: the card keeps its last view,
      // and a deposit in hand can still be checked and claimed.
    }
  }
}

async function checkDeposit(id) {
  update(id, { busy: "checking", error: null });
  try {
    const { swap } = session.swaps[id];
    const tx = await chain.transaction(swap.deposit_txid);
    if (!tx) {
      throw new Error("the relay does not know the deposit");
    }

    // The taker rebuilds the script itself, so the relay cannot fake this.
    const found = taker.findOutput({
      tx_hex: tx.hex,
      address: swap.htlc_address,
    });
    update(id, {
      busy: null,
      verified: { ...found, confirmations: tx.confirmations },
      error: null,
    });
  } catch (error) {
    update(id, { busy: null, verified: null, error: `Deposit check failed: ${error.message}` });
  }
}

async function pay(id) {
  const { swap } = session.swaps[id];
  update(id, { busy: "paying", error: null });

  try {
    if (config.dev_pay) {
      // Stand-in for the taker's wallet. In production the page pays through
      // WebLN and the preimage never touches the coordinator.
      const paid = await api(`/dev/pay/${id}`, "");
      update(id, { busy: null, preimage: paid.preimage, error: null });
    } else if (window.webln) {
      await window.webln.enable();
      const { preimage } = await window.webln.sendPayment(swap.bolt11);
      update(id, { busy: null, preimage, error: null });
    } else {
      throw new Error(
        "no WebLN wallet in this browser and dev pay is off. Pay the invoice from your own wallet, then paste the preimage below.",
      );
    }
  } catch (error) {
    update(id, { busy: null, error: `Payment failed: ${error.message}` });
  }
}

async function claim(id) {
  const entry = session.swaps[id];
  const { swap, preimage, verified } = entry;
  const address = entry.address?.trim();

  update(id, { busy: "claiming", error: null });
  try {
    if (!verified) {
      throw new Error("check the deposit before claiming");
    }
    if (!address) {
      throw new Error("enter the address that should receive your sats");
    }

    // Refuses unless the preimage really unlocks this HTLC.
    const join = taker.checkJoin({
      payment_hash: swap.payment_hash,
      locktime: swap.locktime,
      claim_pubkey: swap.claim_pubkey,
      refund_pubkey: swap.refund_pubkey,
      preimage,
    });

    const signed = taker.claim({
      claim_key: session.key.secret,
      payment_hash: swap.payment_hash,
      locktime: swap.locktime,
      refund_pubkey: swap.refund_pubkey,
      preimage,
      outpoint: `${swap.deposit_txid}:${verified.vout}`,
      value: verified.sats,
      to: address,
      fee: swap.claim_fee_sat,
    });

    const txid = await chain.broadcast(signed.tx_hex);
    update(id, { busy: null, claimed: { txid, address: join.address }, error: null });
  } catch (error) {
    update(id, { busy: null, error: `Claim failed: ${error.message}` });
  }
}

// --- rendering -------------------------------------------------------------

function badge(swap) {
  if (swap.state === "settled") return `<span class="badge good">settled</span>`;
  if (swap.state === "refunded") return `<span class="badge bad">refunded</span>`;
  return `<span class="badge">${swap.state.replace(/_/g, " ")}</span>`;
}

function card(id, entry) {
  const { swap, verified, preimage, claimed, error, busy } = entry;
  if (!swap) return "";

  const rows = [
    `<dt>you receive</dt><dd>${sats(swap.amount_sat)}</dd>`,
    `<dt>you pay</dt><dd>${sats(swap.invoice_sat)} over Lightning</dd>`,
    `<dt>deposit</dt><dd>${short(swap.deposit_txid)}:${swap.deposit_vout ?? "?"} ${
      verified ? `(${sats(verified.sats)}, ${verified.confirmations} conf)` : ""
    }</dd>`,
    `<dt>htlc</dt><dd>${swap.htlc_address}</dd>`,
    `<dt>refund after</dt><dd>height ${swap.locktime}</dd>`,
  ];
  if (preimage) rows.push(`<dt>preimage</dt><dd>${short(preimage, 16, 8)}</dd>`);
  if (claimed) {
    rows.push(`<dt>claimed</dt><dd>${short(claimed.txid)}</dd>`);
  }

  const actions = [];
  if (!verified) {
    actions.push(
      `<button data-act="check" data-id="${id}" ${busy ? "disabled" : ""}>Check the deposit</button>`,
    );
  }
  if (!preimage) {
    actions.push(
      `<button data-act="pay" data-id="${id}" class="primary" ${busy ? "disabled" : ""}>Pay ${sats(
        swap.invoice_sat,
      )}</button>`,
    );
    actions.push(
      `<button data-act="paste" data-id="${id}">I paid elsewhere</button>`,
    );
  } else if (!claimed) {
    actions.push(
      `<input data-addr="${id}" placeholder="address to receive" value="${
        entry.address ?? ""
      }" />`,
    );
    actions.push(
      `<button data-act="claim" data-id="${id}" class="primary" ${busy ? "disabled" : ""}>Claim ${
        verified ? sats(verified.sats - swap.claim_fee_sat) : "on chain"
      }</button>`,
    );
  }

  return `
    <article class="swap">
      <div class="swap-head">
        <span class="swap-id">${short(id, 12, 6)}</span>
        ${badge(swap)}
      </div>
      <dl>${rows.join("")}</dl>
      <div class="actions">${actions.join("")}</div>
      ${busy ? `<p class="quote">${busy}…</p>` : ""}
      ${claimed ? `<p class="ok">Claimed in ${short(claimed.txid)}. That is the swap done.</p>` : ""}
      ${error ? `<p class="error">${error}</p>` : ""}
    </article>`;
}

function render() {
  const ids = Object.keys(session.swaps).reverse();
  $("swaps").innerHTML = ids.map((id) => card(id, session.swaps[id])).join("");
  $("foot").textContent =
    `key ${short(session.key.pubkey, 12, 6)} · relay ${config.relay_url} · ` +
    `${config.dev_pay ? "dev pay is on" : "pay from your own wallet"}`;
}

// --- wiring ----------------------------------------------------------------

$("create").addEventListener("click", create);
$("amount").addEventListener("change", () => askQuote().catch(() => {}));

$("swaps").addEventListener("click", async (event) => {
  const button = event.target.closest("button[data-act]");
  if (!button) return;

  const id = button.dataset.id;
  switch (button.dataset.act) {
    case "check":
      return checkDeposit(id);
    case "pay":
      return pay(id);
    case "claim":
      return claim(id);
    case "paste": {
      const preimage = window.prompt("Paste the preimage your wallet gave you");
      if (preimage) update(id, { preimage: preimage.trim(), error: null });
      return;
    }
  }
});

$("swaps").addEventListener("input", (event) => {
  const id = event.target.dataset.addr;
  if (id) {
    session.swaps[id].address = event.target.value;
    persist();
  }
});

// Recover after a reload: everything this key has going, straight from the
// coordinator, plus whatever the previous session left in local storage.
async function recover() {
  try {
    const { swaps } = await api(`/swaps?claim_pubkey=${session.key.pubkey}`);
    for (const swap of swaps) {
      if (!session.swaps[swap.id]) update(swap.id, { swap });
    }
  } catch (error) {
    $("foot").textContent = `Could not reach the coordinator: ${error.message}`;
  }
}

render();
askQuote().catch(() => {});
await recover();
setInterval(refresh, 3000);
