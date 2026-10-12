// The btc2ln flow's decisions, kept away from the DOM so they can be tested.
//
// The taker funds first here, so the maker gets every chance to build the
// deposit against terms nobody agreed to. Two checks carry the flow: the offer
// has to be the swap the invoice asks for, and the address the taker builds for
// itself has to be the one the maker offered. A maker that gets past either one
// could take the deposit without paying the invoice.
//
// The other rule is the maker's, mirrored here so the taker is told before it
// loses anything: a payment made now expires `cltv_delta` blocks later, and it
// has to expire at least `MIN_MARGIN_BLOCKS` before the deposit can be
// refunded, or the maker will refuse to pay it at all.

/** Blocks the maker insists on between the payment's expiry and the refund. */
export const MIN_MARGIN_BLOCKS = 12;

function fail(message) {
  throw new Error(message);
}

/** Throw unless the maker's offer is the swap the taker asked for. */
export function checkOffer(offer, invoice, refundPubkey) {
  const hash = String(offer.id).toLowerCase();
  if (hash !== String(invoice.payment_hash).toLowerCase()) {
    fail("the maker built the deposit on another payment hash");
  }
  if (!invoice.amount_sat || Number(offer.receive_sat) !== Number(invoice.amount_sat)) {
    fail("the maker is not paying the amount the invoice is for");
  }
  if (String(offer.refund_pubkey).toLowerCase() !== String(refundPubkey).toLowerCase()) {
    fail("the maker would not let this wallet refund the deposit");
  }
  if (!Number(offer.deposit_sat) || !offer.htlc_address || !Number(offer.locktime)) {
    fail("the maker's offer is missing what the deposit needs");
  }
  return offer;
}

/** Throw unless the address the taker builds is the one the maker offered. */
export function checkAddress(offer, rebuilt) {
  if (String(rebuilt).toLowerCase() !== String(offer.htlc_address).toLowerCase()) {
    fail("the maker's address is not the HTLC these terms describe");
  }
  return rebuilt;
}

/** The last height a deposit can still be paid for, by the maker's own rule. */
export function lastFundingHeight(offer) {
  return Number(offer.locktime) - MIN_MARGIN_BLOCKS - Number(offer.cltv_delta);
}

/** What the taker should do next, from the maker's record and the chain. */
export function verdict(offer, height, now = 0) {
  const locktime = Number(offer.locktime);

  if (offer.claim_txid) {
    return {
      state: "claimed",
      note: "the maker took the deposit, and the invoice is settled",
    };
  }
  if (offer.preimage) {
    return {
      state: "paid",
      note: "the invoice is paid; the maker is claiming the deposit",
    };
  }
  if (offer.deposit) {
    if (height >= locktime) {
      return {
        state: "refund",
        note: `the locktime has passed: take the ${offer.deposit_sat} satoshi back`,
      };
    }
    if (height > lastFundingHeight(offer)) {
      return {
        state: "refund_later",
        note: `the window closed before the maker would pay: refund at height ${locktime}`,
      };
    }
    return {
      state: "funded",
      note: "the deposit is in; the maker has not paid the invoice yet",
    };
  }
  if (height >= locktime) {
    return { state: "expired", note: "this offer's locktime has passed" };
  }
  if (now && now >= Number(offer.expires_at)) {
    return {
      state: "invoice_expired",
      note: "the invoice expired; ask the wallet for a new one",
    };
  }
  if (height > lastFundingHeight(offer)) {
    return {
      state: "too_late_to_fund",
      note: `too late to fund: the maker needs room before height ${locktime}`,
    };
  }
  return {
    state: "fund",
    note: `send ${offer.deposit_sat} satoshi to the address, then report the transaction`,
  };
}
