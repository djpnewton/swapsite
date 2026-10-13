// Paying, as the page can offer it: a link to open, a value to copy, and the
// same URI as a code to scan.
//
// Shared by both directions, because a taker paying a Lightning invoice and a
// taker funding a deposit are asking the same three things of a wallet.

import { svg } from "./qr.mjs";

/** A Lightning invoice as a URI a wallet will open. */
export function lightningUri(invoice) {
  return `lightning:${invoice}`;
}

/**
 * A deposit as BIP21, with the amount in BTC.
 *
 * The amount matters: it is what stops a wallet sending less than the HTLC
 * wants, and a deposit that is short is a deposit the maker will not take.
 */
export function bitcoinUri(address, satoshis) {
  const btc = (Number(satoshis) / 1e8)
    .toFixed(8)
    .replace(/0+$/, "")
    .replace(/\.$/, "");

  return `bitcoin:${address}?amount=${btc}`;
}

/**
 * The link, the control that copies, and the code, as a card shows them.
 *
 * The URI is what the link opens and is not printed: a bolt11 is 300 characters
 * that the card already shows in full, and a link only has to be clickable.
 *
 * `raw` is what "copy" puts on the clipboard, which is not the URI when a form
 * wants the bare value: `bitcoin:addr?amount=` is a wallet's to open, not
 * something to paste into someone else's field. `show` prints it above the link
 * for that case.
 */
export function payBlock(uri, { module = 4, raw = uri, show = false } = {}) {
  return (
    (show ? `<p class="raw">${raw}</p>` : "") +
    `<p class="pay-row">` +
    `<a class="pay-link" href="${uri}">link</a>` +
    `<button type="button" class="copy" data-copy="${raw}">copy</button>` +
    `</p>` +
    `<figure class="qr">${svg(uri, { module })}</figure>`
  );
}

/**
 * Make every copy control on the page work.
 *
 * One listener for the document rather than one per card: the cards are rebuilt
 * whenever the poll finds something new, so a listener bound to a card is a
 * listener thrown away with it.
 */
export function installCopy() {
  document.addEventListener("click", async (event) => {
    const button = event.target.closest(".copy");
    if (!button) return;

    const copied = await copyText(button.dataset.copy);
    button.textContent = copied ? "copied" : "copy failed";
    setTimeout(() => {
      button.textContent = "copy";
    }, 1200);
  });
}

/**
 * Copy `text`, the modern way where the browser allows it and the old way where
 * it does not.
 *
 * A refused `writeText` is the reason the fallback exists, so it is not a
 * failure to report: a hidden field and `execCommand` still reach the clipboard
 * in the cases the API turns down.
 */
async function copyText(text) {
  if (navigator.clipboard) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Fallen through to the old route below.
    }
  }

  return legacyCopy(text);
}

/**
 * The old route, for the clipboard API's sake.
 *
 * `navigator.clipboard` needs a secure context, and this page is served over
 * plain http on a network address when it is driven from a phone.
 */
function legacyCopy(text) {
  const field = document.createElement("textarea");
  field.value = text;
  field.setAttribute("readonly", "");
  field.style.position = "fixed";
  field.style.top = "-1000px";
  document.body.appendChild(field);
  field.select();
  const copied = document.execCommand("copy");
  field.remove();
  return copied;
}
