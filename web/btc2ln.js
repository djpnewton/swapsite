// The btc2ln taker, in a page.
//
// The taker funds first, so the whole job here is refusing to hand money to a
// maker that asked for something else: the invoice is decoded in this process
// rather than taken from a server, the offer has to match it field for field,
// and the deposit address is rebuilt from the terms and compared. The rules
// themselves live in btc2ln.mjs, where they are tested without a DOM.
//
// The invoice is pasted rather than made with WebLN: a taker in this direction
// only has to *receive* over Lightning, so any wallet can make the invoice, and
// any on-chain wallet can pay the deposit. WebLN is a convenience, not a
// requirement, which is also what lets this page be driven without one.

import { checkAddress, checkOffer, verdict } from "./btc2ln.mjs";
import { relay } from "./relay.mjs";
import { loadTaker } from "./taker.mjs";

const $ = (id) => document.getElementById(id);

let taker = null;
let chain = null;
let keys = null;
let invoice = null;
let offer = null;
let timer = null;

function say(message, kind = "") {
  $("status").innerHTML = message ? `<p class="${kind}">${message}</p>` : "";
}

async function get(path) {
  const response = await fetch(path);
  const body = await response.json();
  if (!response.ok) throw new Error(body.error ?? `request failed: ${response.status}`);
  return body;
}

async function post(path, payload) {
  const response = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body.error ?? `request failed: ${response.status}`);
  return body;
}

/** Ask for an offer and check every claim in it. */
async function start() {
  const text = $("invoice").value.trim();
  if (!text) throw new Error("paste the invoice you want paid");

  // Read here, so the hash that the deposit is built on is the invoice's own.
  invoice = await taker.bolt11(text);
  if (!invoice.amount_sat) throw new Error("the invoice has to name an amount");

  keys = await taker.keypair();
  // A locktime with room for the maker to pay inside it: it refuses to offer
  // one without, but asking for a short one wastes a round trip.
  const height = await chain.height();
  offer = await post("/btc2ln/offer", {
    invoice: text,
    refund_pubkey: keys.pubkey,
    locktime: height + 200,
  });

  checkOffer(offer, invoice, keys.pubkey);
  const rebuilt = await taker.htlcAddress({
    payment_hash: invoice.payment_hash,
    locktime: offer.locktime,
    claim_pubkey: offer.claim_pubkey,
    refund_pubkey: keys.pubkey,
  });
  checkAddress(offer, rebuilt.address);

  clearInterval(timer);
  timer = setInterval(refresh, 3000);
  render(height);
}

/** The maker's current view of our swap, which is where the state lives. */
async function refresh() {
  if (!offer) return;
  try {
    const height = await chain.height();
    const listed = await get("/btc2ln/swaps");
    const mine = listed.swaps.find((swap) => swap.id === offer.id);
    if (mine) offer = mine;
    render(height);
  } catch (e) {
    say(`cannot refresh just now: ${e.message ?? e}`, "bad");
  }
}

function render(height) {
  const state = verdict(offer, height, Math.floor(Date.now() / 1000));
  const rows = [
    `<dt>deposit</dt><dd>${offer.deposit_sat} satoshi</dd>`,
    `<dt>pay to</dt><dd class="preimage">${offer.htlc_address}</dd>`,
    `<dt>refund after</dt><dd>height ${offer.locktime}</dd>`,
  ];

  if (offer.deposit) rows.push(`<dt>reported</dt><dd>${offer.deposit}</dd>`);
  if (offer.preimage) rows.push(`<dt>preimage</dt><dd class="preimage">${offer.preimage}</dd>`);
  if (offer.claim_txid) rows.push(`<dt>claim</dt><dd>${offer.claim_txid}</dd>`);

  const report = offer.deposit
    ? ""
    : `<label for="outpoint">The transaction id and output, as txid:vout</label>
       <input id="outpoint" type="text" autocomplete="off" spellcheck="false" />
       <button id="report" type="button">I have sent it</button>`;
  $("terms").innerHTML = `<dl>${rows.join("")}</dl>${report}`;
  say(`${state.state}: ${state.note}`, state.state === "funded" ? "ok" : "");

  const button = $("report");
  if (button) {
    button.addEventListener("click", () => {
      const outpoint = $("outpoint").value.trim();
      if (!outpoint) return;
      post("/btc2ln/funded", { id: offer.id, outpoint })
        .then(refresh)
        .catch((e) => say(String(e.message ?? e), "bad"));
    });
  }
}

async function load() {
  try {
    const config = await get("/config");
    chain = relay(config.relay_url);
    taker = await loadTaker("/swapsite_wasm.wasm");

    // A deep link is how a wallet hands the invoice over, and how this page is
    // driven in a test without typing 300 characters.
    const wanted = new URLSearchParams(location.search).get("invoice");
    if (wanted) $("invoice").value = wanted;

    $("start").addEventListener("click", () => {
      start().catch((e) => say(String(e.message ?? e), "bad"));
    });
  } catch (e) {
    say(`this page needs the coordinator: ${e.message ?? e}`, "bad");
  }
}

load();
