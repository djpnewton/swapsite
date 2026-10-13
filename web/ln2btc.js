// The ln2btc taker, in a page.
//
// The taker pays over Lightning first, so the work here is not paying before
// there is something to claim: the deposit is checked against the chain from
// this process, and the claim is signed here with the key made here.
//
// Every piece of cryptography happens in the wasm module. This file is the
// plumbing around it: ask the coordinator for a quote, ask it for a swap, check
// the deposit through the relay, pay, then claim.
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
import { makeList, tabBar } from "/tabs.mjs";
import { lightningUri, payBlock } from "/pay.mjs";
import {
  PAGE_SIZES,
  newPager,
  paginate,
  pagerCommand,
  pagerHtml,
} from "/pager.mjs";

const taker = await loadTaker("/swapsite_wasm.wasm");
const config = await (await fetch("/config")).json();
const chain = relay(config.relay_url);

// The key keeps its old name: renaming it to match this file would orphan the
// swaps already in a browser, preimages included.
const STORAGE = "swapsite.session";
const session = JSON.parse(localStorage.getItem(STORAGE) ?? "{}");
session.key ??= taker.keypair();
session.swaps ??= {};
const persist = () => localStorage.setItem(STORAGE, JSON.stringify(session));

const $ = (id) => document.getElementById(id);
const sats = (n) => `${Number(n).toLocaleString()} sat`;
const short = (text, head = 10, tail = 6) =>
  text.length <= head + tail ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;

/** A preimage is 32 bytes of hex, and nothing else can be signed with. */
const isPreimage = (text) => /^[0-9a-f]{64}$/i.test(text);

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
let height = null;

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
  const ids = Object.keys(session.swaps);
  if (ids.length === 0) return;

  // One height for the whole pass, so two cards cannot disagree about how much
  // time each one has left.
  try {
    height = await chain.height();
  } catch {
    // Without a height a deadline cannot be judged, and a guess would be worse
    // than saying nothing.
    height = null;
  }

  for (const id of ids) {
    const entry = session.swaps[id];
    try {
      const swap = await api(`/swaps/${id}`);
      if (JSON.stringify(swap) !== JSON.stringify(entry.swap)) {
        update(id, { swap });
      }
    } catch {
      // The coordinator being down is not fatal: the card keeps its last view,
      // and a deposit in hand can still be checked and claimed.
    }

    await checkDepositSpent(id);
  }

  render();
  await recoverSwaps();
}

/**
 * Ask the chain whether the deposit is still there.
 *
 * Worth asking because nothing else knows: the maker's records only follow the
 * refund it built itself, so a maker that refunds a deposit it was already paid
 * for leaves no trace anywhere except on chain.
 */
async function checkDepositSpent(id) {
  const entry = session.swaps[id];
  const { deposit_txid, deposit_vout } = entry.swap ?? {};

  if (!deposit_txid || deposit_vout === null || deposit_vout === undefined) return;
  if (entry.claimed) return;

  try {
    const { unspent } = await chain.unspent(`${deposit_txid}:${deposit_vout}`);
    if (unspent !== entry.deposit_unspent) {
      update(id, { deposit_unspent: unspent });
    }
  } catch {
    // A relay that cannot answer does not change what we already believe.
  }
}

/**
 * What this taker can still do about a swap.
 *
 * Computed here rather than asked for: the deadline is public, the height comes
 * from the chain, and whether the preimage is in hand is this page's own
 * knowledge - the part nobody else has.
 */
function verdict(id) {
  const entry = session.swaps[id];
  const { swap, preimage, claimed } = entry;
  if (!swap || claimed || height === null) return null;

  return taker.assess({
    state: swap.state,
    has_preimage: Boolean(preimage),
    // Unknown is assumed unspent, so a page that could not reach the relay
    // still offers the claim rather than calling a recoverable swap dead.
    deposit_unspent: entry.deposit_unspent !== false,
    cltv_delta: swap.cltv_delta,
    deadline: swap.locktime,
    height,
  });
}

/** Swaps with something left to do, so a returned page can just do it. */
function outstanding() {
  return Object.keys(session.swaps)
    .map((id) => ({ id, recovery: verdict(id) }))
    .filter(({ recovery }) => recovery?.actionable);
}

/**
 * Whether this page is finished with a swap, which is what drops it from the
 * list.
 *
 * Three ways to be finished, and they are checked here rather than derived from
 * one field: the taker claimed it, the maker refunded it, or the coins are gone
 * for good. The state is checked as well as the verdict because a page that
 * cannot reach the relay has no verdict at all, and a refunded swap is over
 * whatever the relay says.
 *
 * An expired swap is deliberately *not* finished. Nothing can be done about it,
 * but its card is the only place that says not to pay, and the history shows the
 * maker's state rather than the verdict, so dropping it would lose the warning.
 */
function finished(id) {
  const entry = session.swaps[id];
  return Boolean(
    entry.claimed ||
      entry.swap?.state === "refunded" ||
      verdict(id)?.action.kind === "gone",
  );
}

/**
 * Take the preimage the maker has published.
 *
 * The maker settles to reveal it, so from that moment it is public, and a taker
 * who paid from another wallet can claim with it rather than carrying the
 * secret across by hand. It is only offered on a swap that has already settled,
 * which is exactly when this page is stuck without it - and claiming checks the
 * preimage against the HTLC anyway, so a wrong one makes this page sign
 * nothing.
 */
function adoptRevealedPreimage(id) {
  const entry = session.swaps[id];
  if (entry.preimage || !entry.swap?.preimage) return;
  update(id, { preimage: entry.swap.preimage, error: null });
}

// Addresses typed on this page load. A paste is not a decision to claim, so
// those wait for the button; auto-claiming is for an address that was already
// here, from a session that reloaded with a swap half-done.
const typedAddresses = new Set();

/**
 * Do whatever a returned tab can do by itself.
 *
 * Claiming is unilateral and only ever moves value towards the taker, so it is
 * safe to do without being asked. It cannot be done without a destination
 * address, and that is the taker's to name: a page that recovered after a
 * reload has no address to reuse, so it says so rather than guessing.
 */
async function recoverSwaps() {
  // Before judging anything: a published preimage turns `find_preimage` into a
  // claim, so this is the difference between waiting and acting.
  for (const id of Object.keys(session.swaps)) adoptRevealedPreimage(id);

  for (const { id, recovery } of outstanding()) {
    const entry = session.swaps[id];
    if (entry.busy || entry.claimed || entry.error) continue;
    if (recovery.action.kind === "find_preimage") continue;

    if (!entry.verified) {
      await checkDeposit(id);
      continue;
    }
    if (!entry.address) continue;
    // An address just pasted in is the taker mid-thought, not a taker asking.
    if (typedAddresses.has(id)) continue;
    await claim(id, { auto: true });
  }
}

/**
 * Adopt the swaps the server already holds for this page's key.
 *
 * The way back from a browser that lost its storage: a server record is enough
 * to act on a swap, and a settled one carries the preimage that claims it.
 * Local entries are left exactly as they are, because this page can know more
 * about a swap than the server does - its destination address, its claim, its
 * own view of whether the deposit is still there.
 */
async function adoptServerSwaps() {
  devBusy = true;
  devNote = null;
  renderDev();

  try {
    const { swaps } = await api(`/swaps?claim_pubkey=${session.key.pubkey}`);
    const have = new Set(Object.keys(session.swaps));
    const missing = (swaps ?? []).filter((swap) => !have.has(swap.id));

    for (const swap of missing) session.swaps[swap.id] = { swap };
    if (missing.length > 0) persist();

    devNote = {
      bad: false,
      text:
        missing.length === 0
          ? "Nothing to retrieve: this page already has every swap the server holds for its key."
          : `Retrieved ${missing.length} ${
              missing.length === 1 ? "swap" : "swaps"
            } from the server.`,
    };
  } catch (error) {
    devNote = { bad: true, text: `Retrieved nothing: ${error.message}` };
  } finally {
    devBusy = false;
    renderDev();
  }
}

/**
 * Delete the swaps this page remembers that the server does not know.
 *
 * Regtest only, and deliberately: on a network that matters a swap the server
 * has forgotten is still the taker's, and this page can hold the only copy of
 * the preimage that claims it. On regtest the store is disposable, so those
 * leftovers are just the debris a wiped store leaves in a browser.
 *
 * A server that cannot answer deletes nothing: "unknown" has to mean the server
 * said so, not that it was unreachable. One list is enough to compare against,
 * because this session only ever holds ln2btc swaps - btc2ln keeps its state in
 * the page and nothing on disk.
 */
async function deleteUnknownSwaps() {
  devBusy = true;
  devNote = null;
  renderDev();

  try {
    const { swaps } = await api("/swaps");
    const known = new Set((swaps ?? []).map((swap) => swap.id));
    const unknown = Object.keys(session.swaps).filter((id) => !known.has(id));

    for (const id of unknown) delete session.swaps[id];
    persist();

    devNote = {
      bad: false,
      text:
        unknown.length === 0
          ? "Nothing to delete: the server knows every swap this page remembers."
          : `Deleted ${unknown.length} local ${
              unknown.length === 1 ? "swap" : "swaps"
            } the server does not know.`,
    };
  } catch (error) {
    devNote = { bad: true, text: `Deleted nothing: ${error.message}` };
  } finally {
    devBusy = false;
    renderDev();
  }
}

async function checkDeposit(id) {
  const { swap } = session.swaps[id];
  if (!swap?.deposit_txid) {
    // Nothing is funded until the bond is in, so there is nothing to check yet.
    update(id, { busy: null, error: null });
    return;
  }

  update(id, { busy: "checking", error: null });
  try {
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
  const entry = session.swaps[id];
  const { swap } = entry;
  // Before the deposit exists the only thing to pay is the bond, and paying it
  // reveals nothing: a bond's preimage unlocks no HTLC.
  const bonded = Boolean(swap.deposit_txid);
  update(id, { busy: "paying", error: null });

  try {
    if (bonded) {
      // Both of these are the taker's own protection, and neither is something
      // the maker can be trusted to have done.
      const kind = verdict(id)?.action.kind;
      if (kind === "expired") {
        throw new Error(
          "this swap expired: the maker may refund the deposit now, so paying it could leave your money with nothing to claim",
        );
      }
      if (kind === "too_late_to_pay") {
        throw new Error(
          "the maker's refund would open before a payment made now expires - do not pay into this swap",
        );
      }
      if (!entry.verified) {
        throw new Error("check the deposit before paying");
      }
    }

    if (config.dev_pay) {
      // Stand-in for the taker's wallet. In production the page pays through
      // WebLN and the preimage never touches the coordinator.
      const paid = await api(`/dev/pay/${id}`, "");
      update(id, {
        busy: null,
        error: null,
        ...(paid.preimage ? { preimage: paid.preimage } : {}),
      });
    } else if (window.webln) {
      await window.webln.enable();
      const { preimage } = await window.webln.sendPayment(
        bonded ? swap.bolt11 : swap.bond_bolt11,
      );
      update(id, { busy: null, error: null, ...(bonded ? { preimage } : {}) });
    } else {
      throw new Error(
        "no WebLN wallet in this browser and dev pay is off. Pay the invoice from your own wallet, then paste the preimage below.",
      );
    }
  } catch (error) {
    update(id, { busy: null, error: `Payment failed: ${error.message}` });
  }
}

async function claim(id, options = {}) {
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
    taker.checkJoin({
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
    update(id, {
      busy: null,
      // The destination the taker named, not the HTLC: this is the record of
      // where the coins went, and `checkJoin` only proves the preimage fits.
      claimed: { txid, address, auto: Boolean(options.auto) },
      error: null,
    });
  } catch (error) {
    update(id, { busy: null, error: `Claim failed: ${error.message}` });
  }
}

// --- rendering -------------------------------------------------------------

function badge(swap, recovery) {
  if (swap.state === "settled") return `<span class="badge good">settled</span>`;
  if (swap.state === "refunded") return `<span class="badge bad">refunded</span>`;
  // A closed payment window is not a maker state, but it is the one thing the
  // taker needs to see: a funded swap that can no longer be paid into is over.
  if (recovery?.action.kind === "expired") return `<span class="badge bad">expired</span>`;
  return `<span class="badge">${swap.state.replace(/_/g, " ")}</span>`;
}

function card(id, entry) {
  const { swap, verified, preimage, claimed, error, busy } = entry;
  if (!swap) return "";

  const recovery = verdict(id);
  // Nothing is funded until the bond is in, so this comes before there is a
  // deposit to check or a swap to pay for.
  const bonded = Boolean(swap.deposit_txid);

  const rows = [
    `<dt>you receive</dt><dd>${sats(swap.amount_sat)}</dd>`,
    `<dt>you pay</dt><dd>${sats(swap.invoice_sat)} over Lightning${
      // A swap started before the bond existed simply has none to show.
      swap.bond_sat ? `, plus a ${sats(swap.bond_sat)} bond` : ""
    }</dd>`,
    `<dt>deposit</dt><dd>${
      bonded
        ? `<span class="preimage">${swap.deposit_txid}:${swap.deposit_vout ?? "?"}</span> ${
            verified ? `(${sats(verified.sats)}, ${verified.confirmations} conf)` : ""
          }`
        : "not funded yet"
    }</dd>`,
    `<dt>htlc</dt><dd>${swap.htlc_address}</dd>`,
    `<dt>refund after</dt><dd>height ${swap.locktime}</dd>`,
  ];
  if (recovery?.action.blocks_left !== undefined) {
    rows.push(`<dt>blocks left</dt><dd>${recovery.action.blocks_left}</dd>`);
  }
  if (preimage) rows.push(`<dt>preimage</dt><dd class="preimage">${preimage}</dd>`);
  if (claimed) {
    rows.push(`<dt>claimed</dt><dd><span class="preimage">${claimed.txid}</span></dd>`);
  }

  // A spent deposit cannot be claimed, so offering the buttons would only
  // invite a transaction that is guaranteed to be rejected.
  const gone = recovery?.action.kind === "gone";

  // Either window leaves nothing to pay: one is too short to pay into safely,
  // the other has closed and paying early enough is no longer possible.
  const unpayable = ["too_late_to_pay", "expired"].includes(recovery?.action.kind);

  // Paying cannot help a swap whose deposit is spent either, so that is the one
  // question the rest of the card asks: the buttons and the invoices all appear
  // only while paying is still a move worth making.
  const payable = !gone && !unpayable;

  // The bond comes first and is not returned: it is what makes abandoning a
  // swap cost the taker rather than the maker.
  const bond =
    bonded || !payable
      ? ""
      : `<p class="quote">Pay the bond first, before height ${swap.bond_by}. The
       maker funds nothing until it is in, and it is not returned: it covers its
       on-chain fees either way.</p>
       <pre class="bolt11">${swap.bond_bolt11}</pre>
       ${payBlock(lightningUri(swap.bond_bolt11), { raw: swap.bond_bolt11 })}`;

  // The invoice, shown whether or not this page can pay it. Paying from another
  // wallet is the real flow; the dev shortcut is only a shortcut.
  const invoice =
    bonded && !preimage && swap.bolt11 && payable
      ? `<p class="quote">Pay this invoice, from any wallet on this network:</p>
         <pre class="bolt11">${swap.bolt11}</pre>
         ${payBlock(lightningUri(swap.bolt11), { raw: swap.bolt11 })}`
      : "";

  const actions = [];
  if (payable) {
    if (!bonded) {
      // No deposit yet, so no check and no swap to pay for - only the bond.
      actions.push(
        `<button data-act="pay" data-id="${id}" class="primary" ${busy ? "disabled" : ""}>Pay the ${sats(
          swap.bond_sat,
        )} bond</button>`,
      );
    } else {
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
        // An inline field rather than a modal prompt: a prompt is unstylable, and
        // some hosts do not implement it at all, which makes the only route to a
        // swap paid from another wallet unusable.
        actions.push(
          `<input data-preimage="${id}" placeholder="preimage, if you paid elsewhere" value="${
            entry.typedPreimage ?? ""
          }" />`,
        );
        actions.push(`<button data-act="paste" data-id="${id}">I paid elsewhere</button>`);
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
    }
  }

  return `
    <article class="swap">
      <div class="swap-head">
        <span class="swap-id">${short(id, 12, 6)}</span>
        ${badge(swap, recovery)}
      </div>
      <dl>${rows.join("")}</dl>
      <div class="actions">${actions.join("")}</div>
      ${busy ? `<p class="quote">${busy}…</p>` : ""}
      ${bond}
      ${invoice}
      ${notice(recovery)}
      ${
        claimed
          ? `<p class="ok">Claimed${claimed.auto ? " automatically" : ""} in <span class="preimage">${claimed.txid}</span>. That is the swap done.</p>`
          : ""
      }
      ${error ? `<p class="error">${error}</p>` : ""}
    </article>`;
}

/** The one line that says whether this swap needs the taker right now. */
function notice(recovery) {
  if (!recovery) return "";
  const { action, deadline, urgent } = recovery;

  switch (action.kind) {
    case "claim":
      return urgent
        ? `<p class="error urgent">Act now: ${action.blocks_left} blocks left before the maker is allowed to refund this deposit.</p>`
        : `<p class="quote">${action.blocks_left} blocks to claim. Until then the maker cannot touch the deposit.</p>`;
    case "racing":
      return `<p class="error urgent">The refund path is open, so the maker can take this deposit back at any moment. Claim now - a claim still wins, but only if it gets there first.</p>`;
    case "find_preimage":
      return `<p class="error${urgent ? " urgent" : ""}">Your payment settled, so the preimage is public - the maker publishes it when it settles and this clears by itself. If the maker cannot be reached, paste it from the wallet that paid, before height ${deadline}.</p>`;
    case "too_late_to_pay":
      return `<p class="error">Only ${action.blocks_left} blocks until the maker may refund, and a payment made now would not expire before that. Paying into this swap could leave your payment settled with the deposit already refunded, so do not.</p>`;
    case "expired":
      return `<p class="error">This swap expired: the refund path is open, so the maker can withdraw the deposit at any moment, and a payment made now would not expire before that. Do not pay into it.</p>`;
    case "gone":
      return `<p class="error">This deposit has been spent, so there is no longer anything to claim. If you claimed it yourself, your coins are at the address you named. If you did not, the maker refunded it and the payment you made went with it.</p>`;
    default:
      return "";
  }
}

let lastMarkup = "";

const pager = newPager();

// The list's own state: which tab, and which swaps it has let go. A different
// question from the history's tabs - that one covers every swap the server
// knows, this one only the swaps this browser remembers.
const list = makeList((id) => finished(id));

// What the last regtest tool said, and whether one is running.
let devNote = null;
let devBusy = false;

/**
 * The regtest-only tools, written straight into their box at the foot of the
 * page.
 *
 * Not part of the list markup: they are not swaps, and nothing about them
 * changes when the poll repaints. Both are session bookkeeping against the
 * server's copy, and both are kept to regtest - elsewhere a swap the server has
 * forgotten is still the taker's, and this page can hold the only preimage that
 * claims it.
 */
function renderDev() {
  $("retrieve").disabled = devBusy;
  $("cleanup").disabled = devBusy;
  $("dev-note").className = devNote ? (devNote.bad ? "error" : "ok") : "quote";
  $("dev-note").textContent = devNote?.text ?? "";
}

/**
 * The tabs, the cards for this page's swaps, and the pager.
 *
 * Newest first, in every tab. A swap leaves the list when there is nothing left
 * to say about it, except an expired one: its card is the only place that says
 * not to pay, so it is not the page's to drop.
 */
function listMarkup() {
  const ids = Object.keys(session.swaps).reverse();
  const now = Date.now();
  const counts = list.counts(ids, now);
  const view = paginate(list.pick(ids, now), pager);

  if (view.total === 0) {
    const note = ids.length === 0 ? "No swaps yet." : `Nothing ${list.tab} here.`;
    return `${tabBar(list.tab, counts)}<p class="quote">${note}</p>`;
  }

  const cards = view.rows.map((id) => card(id, session.swaps[id])).join("");
  // A pager on a list of three is noise: it arrives with the second page.
  const controls = view.total > PAGE_SIZES[0] ? pagerHtml(view) : "";

  return `${tabBar(list.tab, counts)}${cards}${controls}`;
}

function render() {
  const markup = listMarkup();

  // Only touch the DOM when the markup actually changed. A poll runs every few
  // seconds, and replacing the list each time would take the cursor out of the
  // address field the taker is in the middle of typing into.
  if (markup !== lastMarkup) {
    lastMarkup = markup;
    $("swaps").innerHTML = markup;
  }

  $("foot").textContent =
    `key ${short(session.key.pubkey, 12, 6)} · relay ${config.relay_url} · ` +
    `${config.dev_pay ? "dev pay is on" : "pay from your own wallet"}`;
}

// --- wiring ----------------------------------------------------------------

$("create").addEventListener("click", create);
$("amount").addEventListener("change", () => askQuote().catch(() => {}));

// The regtest-only box, and none of it on a network that matters.
$("dev").hidden = config.network !== "regtest";
$("retrieve").addEventListener("click", adoptServerSwaps);
$("cleanup").addEventListener("click", deleteUnknownSwaps);

$("swaps").addEventListener("click", async (event) => {
  const chosen = event.target.closest("button[data-tab]");
  if (chosen) {
    list.choose(chosen.dataset.tab);
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

  const id = button.dataset.id;
  switch (button.dataset.act) {
    case "check":
      return checkDeposit(id);
    case "pay":
      return pay(id);
    case "claim":
      return claim(id);
    case "paste": {
      const typed = (session.swaps[id].typedPreimage ?? "").trim();
      if (!typed) {
        update(id, { error: "paste the preimage your wallet gave you first" });
        return;
      }
      if (!isPreimage(typed)) {
        update(id, {
          error:
            typed.length === 64
              ? "a preimage is 64 hex characters and this one has something else in it"
              : `a preimage is 64 hex characters and this is ${typed.length}, so copy the whole value`,
        });
        return;
      }
      update(id, { preimage: typed, error: null });
      return;
    }
  }
});

$("swaps").addEventListener("input", (event) => {
  const { addr, preimage } = event.target.dataset;

  // Held in the session so a reload does not lose a paste in progress.
  if (addr) {
    session.swaps[addr].address = event.target.value;
    typedAddresses.add(addr);
  }
  if (preimage) session.swaps[preimage].typedPreimage = event.target.value;
  if (addr || preimage) persist();
});

// The page size is a select, so it arrives as a change rather than a click.
$("swaps").addEventListener("change", (event) => {
  const command = pagerCommand(event.target.closest("[data-size]"));
  if (!command?.size) return;

  pager.size = command.size;
  pager.page = 1;
  render();
});

// Recover after a reload: everything this key has going, straight from the
// coordinator, plus whatever the previous session left in local storage.
async function recover() {
  try {
    const { swaps } = await api(`/swaps?claim_pubkey=${session.key.pubkey}`);
    for (const swap of swaps) {
      // Not only for swaps this page has not seen: the coordinator refreshes
      // each one from the maker, and a stored copy can be older than the terms
      // the swap actually has.
      update(swap.id, { swap });
    }
  } catch (error) {
    $("foot").textContent = `Could not reach the coordinator: ${error.message}`;
  }
}

render();
askQuote().catch(() => {});
await recover();
// A returned tab with a preimage in hand claims without being asked: waiting
// for the taker to notice a countdown is how a swap gets lost.
await recoverSwaps();
setInterval(refresh, 3000);
