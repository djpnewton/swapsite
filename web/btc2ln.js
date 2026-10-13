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
//
// Offers are kept in storage rather than in memory, and shown as the same list
// of cards the other direction shows. What each one carries that the maker's
// record does not is the refund key: it is the only way back to the deposit
// after the locktime, so a reload that forgot it would leave the money in the
// HTLC with nothing able to take it out.

import { checkAddress, checkOffer, refundProblem, verdict } from "./btc2ln.mjs";
import { PAGE_SIZES, newPager, paginate, pagerCommand, pagerHtml } from "./pager.mjs";
import { bitcoinUri, payBlock } from "./pay.mjs";
import { makeList, tabBar } from "./tabs.mjs";
import { relay } from "./relay.mjs";
import { loadTaker } from "./taker.mjs";

const $ = (id) => document.getElementById(id);
const sats = (n) => `${Number(n).toLocaleString()} sat`;
const short = (text, head = 12, tail = 6) =>
  text.length <= head + tail ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;
const unixNow = () => Math.floor(Date.now() / 1000);

/** A transaction id is 32 bytes of hex, and nothing else names a deposit. */
const isTxid = (text) => /^[0-9a-f]{64}$/.test(text);

/** The least a refund pays in fees: a transaction no node relays is no refund. */
const MIN_REFUND_FEE = 1000;

/**
 * What a refund costs to spend, and what is left of the deposit after it.
 *
 * The offer's own figure for one spend of this deposit, and never less than the
 * module's default: a maker that set it to nothing would leave a transaction no
 * node agrees to relay.
 */
const refundFee = (offer) => Math.max(Number(offer.claim_fee_sat) || 0, MIN_REFUND_FEE);
const refundNet = (offer) => Number(offer.deposit_sat) - refundFee(offer);

const STORAGE = "swapsite.btc2ln";
const session = JSON.parse(localStorage.getItem(STORAGE) ?? "{}");
session.offers ??= {};
const persist = () => localStorage.setItem(STORAGE, JSON.stringify(session));

let taker = null;
let chain = null;
let height = null;
let timer = null;

const list = makeList((id) => over(id));
const pager = newPager();
let lastMarkup = "";

function say(message, kind = "") {
  // The same line the other direction uses for its quote, so the two cards read
  // alike: a class only when there is something to say about it.
  const line = $("status");
  line.textContent = message;
  line.className = ["quote", kind === "bad" ? "error" : kind].filter(Boolean).join(" ");
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
  const invoice = await taker.bolt11(text);
  if (!invoice.amount_sat) throw new Error("the invoice has to name an amount");

  const key = await taker.keypair();
  // A locktime with room for the maker to pay inside it: it refuses to offer
  // one without, but asking for a short one wastes a round trip.
  height = await chain.height();
  const offer = await post("/btc2ln/offer", {
    invoice: text,
    refund_pubkey: key.pubkey,
    locktime: height + 200,
  });

  checkOffer(offer, invoice, key.pubkey);
  const rebuilt = await taker.htlcAddress({
    payment_hash: invoice.payment_hash,
    locktime: offer.locktime,
    claim_pubkey: offer.claim_pubkey,
    refund_pubkey: key.pubkey,
  });
  checkAddress(offer, rebuilt.address);

  // Stored before anything is paid: the key above is what the deposit comes back
  // to, and it exists nowhere else.
  session.offers[offer.id] = { offer, key };
  persist();
  say("");
  render();
}

/** Every offer this page holds, refreshed from the maker's own records. */
async function refresh() {
  if (Object.keys(session.offers).length === 0) return;

  // One height for the whole pass, so two cards cannot disagree about the time
  // each one has left.
  try {
    height = await chain.height();
  } catch {
    // Without a height no deadline can be judged, and a guess would be worse
    // than saying nothing.
    height = null;
  }

  try {
    const listed = await get("/btc2ln/swaps");
    for (const swap of listed.swaps ?? []) {
      const entry = session.offers[swap.id];
      // Only ours: the maker lists every swap it has, and this page is not
      // entitled to the others.
      if (entry) entry.offer = swap;
    }
    persist();
  } catch (e) {
    say(`cannot refresh just now: ${e.message ?? e}`, "bad");
  }

  render();
}

/** What the page should say about an offer, or null while the height is unknown. */
function stateOf(entry) {
  // A refund this page built outranks the maker's record, which cannot see that
  // the refund path was taken: its copy still says the deposit is waiting.
  if (entry?.refund) {
    return { state: "refunded", note: "you took the deposit back after the locktime" };
  }

  if (!entry?.offer || height === null) return null;
  return verdict(entry.offer, height, unixNow());
}

/**
 * States that leave the taker nothing to do, so the list can file them away.
 *
 * Expired belongs here, unlike on the ln2btc page: it means the locktime passed
 * with no deposit, so there are no coins to keep an eye on. A deposit that is
 * waiting out its locktime is refund_later, and that stays on the list.
 */
const OVER = new Set([
  "claimed",
  "refunded",
  "expired",
  "invoice_expired",
  "too_late_to_fund",
]);

/**
 * Whether this offer is over with.
 *
 * One whose deposit is waiting to be refunded is not: the maker has the money,
 * the taker has a claim on it, and that card is the one worth showing.
 */
function over(id) {
  const state = stateOf(session.offers[id]);
  return Boolean(state && OVER.has(state.state));
}

const LABELS = {
  fund: "offered",
  funded: "funded",
  paid: "paid",
  claimed: "claimed",
  refund: "refund now",
  refund_later: "refund later",
  refunded: "refunded",
  expired: "expired",
  invoice_expired: "invoice expired",
  too_late_to_fund: "too late to fund",
};

const GOOD = new Set(["claimed", "paid", "refunded"]);
const BAD = new Set(["refund", "refund_later", "expired", "invoice_expired", "too_late_to_fund"]);

function badge(state) {
  if (!state) return `<span class="badge">…</span>`;
  const kind = GOOD.has(state.state) ? " good" : BAD.has(state.state) ? " bad" : "";
  return `<span class="badge${kind}">${LABELS[state.state] ?? state.state}</span>`;
}

/**
 * What a taker does about a deposit the maker never paid for.
 *
 * The refund is unilateral - this page's own key spending the deposit after the
 * locktime - so it needs a destination and that key, and nothing from the maker.
 * A page that has lost the key says so rather than offering a button that cannot
 * work: the key was made when the offer was accepted, and only here was it kept.
 */
function refundBlock(id, entry, offer) {
  if (!entry.key) {
    return `<p class="error">This page does not hold the refund key for this swap, so the deposit cannot be taken back from here.</p>`;
  }

  return (
    `<div class="row">
       <label for="refund-${id}">Address the deposit goes back to</label>
       <input id="refund-${id}" data-refund="${id}" value="${
         entry.refundAddress ?? ""
       }" autocomplete="off" spellcheck="false" />
     </div>
     <button data-act="refund" data-id="${id}" class="primary">Take the ${sats(
       refundNet(offer),
     )} back</button>`
  );
}

function card(id, entry) {
  const { offer } = entry;
  const state = stateOf(entry);
  const rows = [
    `<dt>you receive</dt><dd>${sats(offer.receive_sat)} over Lightning</dd>`,
    `<dt>you deposit</dt><dd>${sats(offer.deposit_sat)} on chain</dd>`,
    `<dt>pay to</dt><dd class="preimage">${offer.htlc_address}</dd>`,
    `<dt>refund after</dt><dd>height ${offer.locktime}</dd>`,
  ];

  if (offer.deposit) {
    rows.push(`<dt>reported</dt><dd><span class="preimage">${offer.deposit}</span></dd>`);
  }
  if (offer.preimage) rows.push(`<dt>preimage</dt><dd class="preimage">${offer.preimage}</dd>`);
  if (offer.claim_txid) {
    rows.push(`<dt>claim</dt><dd><span class="preimage">${offer.claim_txid}</span></dd>`);
  }
  if (entry.refund) {
    rows.push(`<dt>refunded</dt><dd><span class="preimage">${entry.refund.txid}</span></dd>`);
  }

  // Paying and reporting only matter while the deposit is missing and there is
  // still a window to send it in.
  const asking = !offer.deposit && state?.state === "fund";
  const link = asking
    ? `<p class="quote">Pay it from any wallet:</p>
       ${payBlock(bitcoinUri(offer.htlc_address, offer.deposit_sat), {
         raw: offer.htlc_address,
         show: true,
       })}`
    : "";
  const report = asking
    ? `<div class="row">
         <label for="txid-${id}">The transaction id</label>
         <input id="txid-${id}" data-txid="${id}" value="${
           entry.txid ?? ""
         }" autocomplete="off" spellcheck="false" />
       </div>
       <button data-act="report" data-id="${id}" class="primary">I have sent it</button>`
    : "";

  // Once the locktime has passed the deposit is the taker's again, and this is
  // the one state where the maker has no say in what happens to it.
  const refund = state?.state === "refund" ? refundBlock(id, entry, offer) : "";

  return `
    <article class="swap">
      <div class="swap-head">
        <span class="swap-id">${short(id)}</span>
        ${badge(state)}
      </div>
      <dl>${rows.join("")}</dl>
      ${link}
      ${report ? `<div class="actions">${report}</div>` : ""}
      ${refund ? `<div class="actions">${refund}</div>` : ""}
      ${state ? `<p class="${GOOD.has(state.state) ? "ok" : "quote"}">${state.note}</p>` : ""}
      ${entry.error ? `<p class="error">${entry.error}</p>` : ""}
    </article>`;
}

/** The tabs over this page's offers, the cards, and the pager. */
function listMarkup() {
  const ids = Object.keys(session.offers).reverse();
  const now = Date.now();
  const counts = list.counts(ids, now);
  const view = paginate(list.pick(ids, now), pager);

  if (view.total === 0) {
    const note = ids.length === 0 ? "No swaps yet." : `Nothing ${list.tab} here.`;
    return `${tabBar(list.tab, counts, "data-btab")}<p class="quote">${note}</p>`;
  }

  const cards = view.rows.map((id) => card(id, session.offers[id])).join("");
  // A pager on a list of three is noise: it arrives with the second page.
  const controls = view.total > PAGE_SIZES[0] ? pagerHtml(view) : "";

  return `${tabBar(list.tab, counts, "data-btab")}${cards}${controls}`;
}

function render() {
  const markup = listMarkup();

  // Only rebuild when it changed, so a poll does not take the cursor out of the
  // transaction id field the taker is pasting into.
  if (markup !== lastMarkup) {
    lastMarkup = markup;
    $("btc2ln-swaps").innerHTML = markup;
  }
}

/**
 * Which output of that transaction pays the HTLC.
 *
 * Only the transaction id is asked of the taker, and the maker is told an
 * outpoint rather than left to search: the search happens here, where the
 * script is rebuilt from the offer's own terms, so a relay answering with the
 * wrong transaction cannot make this page report an output that is not the
 * swap's.
 */
async function fundedOutpoint(entry, txid) {
  if (!taker || !chain) throw new Error("this page is still starting up");

  const tx = await chain.transaction(txid);
  if (!tx) throw new Error("the relay does not know that transaction");

  const found = taker.findOutput({
    tx_hex: tx.hex,
    address: entry.offer.htlc_address,
  });

  // Computed by the wasm from the bytes it was handed, so this catches a relay
  // that answered with a different transaction than the one asked for.
  if (found.txid !== txid) {
    throw new Error(`the relay answered with ${short(found.txid)} for ${short(txid)}`);
  }

  return `${found.txid}:${found.vout}`;
}

/** Report the transaction that funded the deposit, by transaction id alone. */
async function report(id) {
  const entry = session.offers[id];
  const typed = (entry?.txid ?? "").trim().toLowerCase();

  if (!isTxid(typed)) {
    entry.error = typed
      ? "a transaction id is 64 hex characters, so copy the whole value"
      : "paste the transaction id first";
    persist();
    return render();
  }

  try {
    entry.error = null;
    await post("/btc2ln/funded", { id, outpoint: await fundedOutpoint(entry, typed) });
  } catch (e) {
    entry.error = String(e.message ?? e);
  }

  persist();
  await refresh();
}

/**
 * Take the deposit back, once the maker has run out of time to pay for it.
 *
 * Signing here rather than asking anyone: the key is this page's, the locktime
 * is public, and the relay only has to carry the transaction.
 */
async function refund(id) {
  const entry = session.offers[id];
  const { offer } = entry;
  const address = (entry.refundAddress ?? "").trim();

  try {
    entry.error = null;
    if (!address) throw new Error("enter the address the deposit should go back to");

    // One relay answer serves both the check and the transaction: whether the
    // deposit is still there, and what it is worth now.
    const deposit = await chain.unspent(offer.deposit);
    const problem = refundProblem(offer, height, deposit);
    if (problem) throw new Error(problem);

    const signed = taker.refund({
      refund_key: entry.key.secret,
      // The id is the payment hash: the maker names a swap after the HTLC it
      // builds, and this page checked that when the offer arrived.
      payment_hash: offer.id,
      locktime: Number(offer.locktime),
      claim_pubkey: offer.claim_pubkey,
      outpoint: offer.deposit,
      // The relay's own figure where it has one, the maker's where it does not.
      value: Number(deposit?.sats ?? offer.deposit_sat),
      to: address,
      fee: refundFee(offer),
    });

    entry.refund = { txid: await chain.broadcast(signed.tx_hex), address };
  } catch (e) {
    entry.error = String(e.message ?? e);
  }

  persist();
  await refresh();
}

$("btc2ln-swaps").addEventListener("click", async (event) => {
  const chosen = event.target.closest("button[data-btab]");
  if (chosen) {
    list.choose(chosen.dataset.btab);
    pager.page = 1; // a new tab starts at its own first page
    return render();
  }

  const control = event.target.closest("[data-pager]");
  if (control) {
    const command = pagerCommand(control);
    if (command?.move) pager.page += command.move;
    return render();
  }

  const button = event.target.closest("button[data-act]");
  if (!button) return;

  if (button.dataset.act === "refund") return refund(button.dataset.id);
  if (button.dataset.act === "report") return report(button.dataset.id);
});

// The page size is a select, so it arrives as a change rather than a click.
$("btc2ln-swaps").addEventListener("change", (event) => {
  const command = pagerCommand(event.target.closest("[data-size]"));
  if (!command?.size) return;

  pager.size = command.size;
  pager.page = 1;
  render();
});

$("btc2ln-swaps").addEventListener("input", (event) => {
  const { txid, refund: refundId } = event.target.dataset;
  if (!txid && !refundId) return;

  // Held in the session so a poll does not lose a paste in progress.
  if (txid) session.offers[txid].txid = event.target.value;
  if (refundId) session.offers[refundId].refundAddress = event.target.value;
  persist();
});

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

    render();
    await refresh();
    timer = setInterval(refresh, 3000);
  } catch (e) {
    say(`this page needs the coordinator: ${e.message ?? e}`, "bad");
  }
}

load();
