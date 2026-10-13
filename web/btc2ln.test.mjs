import assert from "node:assert/strict";
import test from "node:test";

import {
  checkAddress,
  checkOffer,
  lastFundingHeight,
  MIN_MARGIN_BLOCKS,
  refundProblem,
  verdict,
} from "./btc2ln.mjs";

const HASH = "fce34a0b88e454a6ac69a4fc438e2aab4b73469b881a85a8c78b2080a42cdc00";
const REFUND = "0268fac3e0a7ebaf8dd3c83cd40740de8203af259f8cf5efbe5f21b36ba6c4667a";
const ADDRESS = "bcrt1qsw70nqguratfqx90ks833rwknrmjdljegdfjp972jpqel22zcyvqhfqxla";

const invoice = { payment_hash: HASH, amount_sat: 20_000 };

/// The offer the test maker returned for that invoice, trimmed to the fields
/// the page reads.
function offer(overrides = {}) {
  return {
    id: HASH,
    receive_sat: 20_000,
    deposit_sat: 22_000,
    htlc_address: ADDRESS,
    refund_pubkey: REFUND,
    locktime: 4_200,
    cltv_delta: 80,
    expires_at: 1_791_920_731,
    deposit: null,
    preimage: null,
    claim_txid: null,
    ...overrides,
  };
}

test("an offer that matches the invoice is accepted", () => {
  assert.equal(checkOffer(offer(), invoice, REFUND).id, HASH);
});

test("an offer on another hash is refused", () => {
  const other = "00".repeat(32);
  assert.throws(() => checkOffer(offer({ id: other }), invoice, REFUND), /another payment hash/);
});

test("an offer that pays a different amount is refused", () => {
  assert.throws(() => checkOffer(offer({ receive_sat: 19_999 }), invoice, REFUND), /not paying the amount/);
});

test("an offer that would not refund to this wallet is refused", () => {
  const theirs = "03".repeat(33);
  assert.throws(() => checkOffer(offer({ refund_pubkey: theirs }), invoice, REFUND), /let this wallet refund/);
});

test("an offer missing what the deposit needs is refused", () => {
  assert.throws(() => checkOffer(offer({ htlc_address: "" }), invoice, REFUND), /missing/);
  assert.throws(() => checkOffer(offer({ locktime: 0 }), invoice, REFUND), /missing/);
});

test("an address that is not the terms is refused", () => {
  assert.equal(checkAddress(offer(), ADDRESS), ADDRESS);
  // bech32 spells lowercase, but a maker that shouts is not signed up to a
  // different script.
  assert.equal(checkAddress(offer(), ADDRESS.toUpperCase()), ADDRESS.toUpperCase());
  assert.throws(() => checkAddress(offer(), "bcrt1qother"), /not the HTLC/);
});

test("the last funding height mirrors the maker's own rule", () => {
  const terms = offer();
  assert.equal(lastFundingHeight(terms), 4_200 - MIN_MARGIN_BLOCKS - 80);
  assert.equal(verdict(terms, 4_108).state, "fund", "the last height still funds");
  assert.equal(verdict(terms, 4_109).state, "too_late_to_fund");
});

test("a deposit in time is waited on", () => {
  const funded = offer({ deposit: "ab".repeat(32) + ":0" });
  assert.equal(verdict(funded, 4_000).state, "funded");
});

test("a deposit whose window closed is refunded at the locktime", () => {
  const funded = offer({ deposit: "ab".repeat(32) + ":0" });
  assert.equal(verdict(funded, 4_109).state, "refund_later");
  assert.equal(verdict(funded, 4_200).state, "refund");
});

test("a paid invoice is claimed, then done", () => {
  const funded = offer({ deposit: "ab".repeat(32) + ":0" });
  assert.equal(verdict({ ...funded, preimage: "cd".repeat(32) }, 4_000).state, "paid");
  assert.equal(
    verdict({ ...funded, preimage: "cd".repeat(32), claim_txid: "ef".repeat(32) }, 4_000).state,
    "claimed",
  );
});

test("an offer whose locktime passed is expired", () => {
  assert.equal(verdict(offer(), 4_200).state, "expired");
});

test("an invoice that expired is named as such", () => {
  assert.equal(verdict(offer(), 4_000, 1_791_920_731).state, "invoice_expired");
});

/** The deposit the maker reported, as the page learns it. */
const DEPOSIT = `${"ab".repeat(32)}:0`;
const FUNDED = offer({ deposit: DEPOSIT });
const UNSPENT = { unspent: true, sats: 22_000 };

test("a funded deposit can be taken back once the locktime has passed", () => {
  assert.equal(refundProblem(FUNDED, 4_200, UNSPENT), null, "on the height itself");
  assert.equal(refundProblem(FUNDED, 4_900, UNSPENT), null, "and long after");

  // A relay that cannot answer must not cost the taker its own money: the
  // transaction is built, and fails at the node if the deposit was really gone.
  assert.equal(refundProblem(FUNDED, 4_200, null), null);
});

test("a deposit is not refundable before the locktime", () => {
  assert.match(refundProblem(FUNDED, 4_199, UNSPENT), /opens at height 4200, 1 block away/);
  assert.match(refundProblem(FUNDED, 3_000, UNSPENT), /opens at height 4200, 1200 blocks away/);
});

test("a deposit the maker claimed is not refundable", () => {
  const claimed = offer({ deposit: DEPOSIT, claim_txid: "ef".repeat(32) });
  assert.match(refundProblem(claimed, 9_999, UNSPENT), /already claimed/);
});

test("a swap the maker paid for is not refundable", () => {
  const paid = offer({ deposit: DEPOSIT, preimage: "cd".repeat(32) });
  assert.match(refundProblem(paid, 9_999, UNSPENT), /maker's to claim/);
});

test("a claim decides it even when no preimage is known here", () => {
  const claimed = offer({ deposit: DEPOSIT, claim_txid: "ef".repeat(32) });
  assert.doesNotMatch(refundProblem(claimed, 9_999, UNSPENT), /null/);
});

test("a swap with no deposit has nothing to take back", () => {
  assert.match(refundProblem(offer(), 9_999, null), /nothing to take back/);
});

test("a deposit that is already spent is refused", () => {
  const spent = { unspent: false, sats: null };
  assert.match(refundProblem(FUNDED, 9_999, spent), /already been spent/);
});

test("an unknown height is refused rather than guessed", () => {
  assert.match(refundProblem(FUNDED, null, UNSPENT), /height is unknown/);
  assert.match(refundProblem(FUNDED, undefined, UNSPENT), /height is unknown/);
});
