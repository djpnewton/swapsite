#!/usr/bin/env bash
#
# One complete swap, end to end, on regtest.
#
# Direction: the taker pays Lightning and receives on-chain (ln2btc). The
# maker issues a hold invoice and funds an HTLC; the taker pays
# the invoice and claims the HTLC with the preimage Lightning reveals.
#
# Neither side can take the other's value: the maker cannot claim the HTLC
# (wrong key), the taker cannot spend the HTLC without paying (no preimage), and
# the maker's deposit comes back if it never settles.

source "$(dirname "$0")/lib.sh"

AMOUNT="${AMOUNT:-30000}"   # sats the taker pays over Lightning
CLAIM_FEE=2000

echo "== roles =="
echo "maker: issues the hold invoice, funds the on-chain HTLC"
echo "taker: pays the invoice, claims the on-chain HTLC"

OUTBOUND=$(ln_outbound_sat taker)
[ -n "$OUTBOUND" ] || fail "could not read the taker's Lightning balance"
require_outbound "$AMOUNT" taker
echo "taker spendable outbound: $OUTBOUND sat"

# The taker's claim key. Only the taker sees the secret.
TAKER=$(swapx keygen)
TAKER_SECRET=$(printf '%s\n' "$TAKER" | field secret)
TAKER_PUBKEY=$(printf '%s\n' "$TAKER" | field pubkey)

# The maker's refund key. Only the maker sees the secret.
MAKER=$(swapx keygen)
MAKER_SECRET=$(printf '%s\n' "$MAKER" | field secret)
MAKER_PUBKEY=$(printf '%s\n' "$MAKER" | field pubkey)

# The maker picks the preimage and keeps it. The taker does not learn it until
# Lightning settles.
GENERATED=$(swapx rand-preimage 2>/dev/null)
PREIMAGE=$(printf '%s\n' "$GENERATED" | field preimage)
HASH=$(printf '%s\n' "$GENERATED" | field payment_hash)
LOCKTIME=$(( $(height) + 30 ))

# --- the maker commits -----------------------------------------------------

echo
echo "== maker: hold invoice over Lightning =="
INVOICE=$(swapx ln add-hold --node maker --preimage "$PREIMAGE" --amount "$AMOUNT" \
  --memo swap 2>/dev/null | field bolt11)
[ -n "$INVOICE" ] || fail "maker could not create a hold invoice"
echo "bolt11:        ${INVOICE:0:40}..."
echo "payment hash:  $HASH"
echo "amount:        $AMOUNT sat, held until settled"

echo
echo "== maker: fund the on-chain HTLC =="
ADDR=$(swapx htlc-address --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$MAKER_PUBKEY" 2>/dev/null)
# Deposit `AMOUNT` plus the claim fee, so the taker nets exactly what it paid.
read -r FUND_TX FUND_VOUT FUND_SATS <<< "$(fund_htlc "$ADDR" $(( AMOUNT + CLAIM_FEE )))"
echo "htlc address:  $ADDR"
echo "claim path:    taker, with the preimage"
echo "refund path:   maker, from height $LOCKTIME"
echo "funded:        $FUND_TX:$FUND_VOUT ($FUND_SATS sat)"

# --- the taker checks before committing ------------------------------------

echo
echo "== taker: verify the HTLC before paying =="
# The taker rebuilds the address from public values only: its own pubkey, the
# maker's pubkey, the payment hash and the locktime.
EXPECTED=$(swapx htlc-address --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$MAKER_PUBKEY" 2>/dev/null)
[ "$EXPECTED" = "$ADDR" ] || fail "htlc address mismatch"
[ -n "$(find_output "$FUND_TX" "$EXPECTED")" ] \
  || fail "the funding output does not pay the expected HTLC"
echo "on-chain leg matches:  confirmed"

echo
echo "== taker: pay the hold invoice =="
swapx ln pay --node taker --invoice "$INVOICE" \
  > "$WORK/pay.out" 2> "$WORK/pay.err" < /dev/null &
PAY_PID=$!

STATE=$(await_invoice_state "$HASH" ACCEPTED maker) \
  || fail "maker never saw the payment held (state=$STATE)"
echo "maker invoice: $STATE (funds held, preimage still hidden)"

# --- settle, reveal, claim -------------------------------------------------

echo
echo "== maker: settle =="
# The on-chain deposit is confirmed and the payer's funds are held, so the maker
# settles. Settling is what reveals the preimage.
swapx ln settle --node maker --preimage "$PREIMAGE" > /dev/null
STATE=$(await_invoice_state "$HASH" SETTLED maker) || fail "invoice did not settle"
echo "maker invoice: $STATE"

wait "$PAY_PID" || fail "taker payment failed"
REVEALED=$(field preimage < "$WORK/pay.out")
[ -n "$REVEALED" ] || fail "taker learned no preimage"
echo "taker learned: ${REVEALED:0:16}... (it had no preimage before the settle)"

echo
echo "== taker: claim the on-chain leg =="
# This is the atomic link. The command refuses if the preimage does not unlock
# the HTLC.
swapx check-join --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$MAKER_PUBKEY" \
  --preimage "$REVEALED" > /dev/null

DEST=$(btc getnewaddress)
RAW=$(swapx claim --claim-key "$TAKER_SECRET" --refund-pubkey "$MAKER_PUBKEY" \
  --payment-hash "$HASH" --locktime "$LOCKTIME" --preimage "$REVEALED" \
  --outpoint "$FUND_TX:$FUND_VOUT" --value "$FUND_SATS" --to "$DEST" \
  --fee "$CLAIM_FEE" 2>/dev/null)
CLAIM_TX=$(btc sendrawtransaction "$RAW")
mine 1
echo "claim tx:      $CLAIM_TX"
echo "confirmations: $(confirmations "$CLAIM_TX")"
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || fail "claim did not confirm"

read -r _ RECEIVED <<< "$(find_output "$CLAIM_TX" "$DEST")"
# Equal value crossed the swap: the taker nets exactly what it paid.
[ "$RECEIVED" = "$AMOUNT" ] || fail "taker received $RECEIVED sat, expected $AMOUNT"

echo
echo "swap done: $AMOUNT sat moved Lightning -> on-chain"
echo "           maker received $AMOUNT sat over Lightning"
echo "           taker received $RECEIVED sat on-chain"

# --- failure path: the maker backs out -------------------------------------

echo
echo "== failure path: maker cancels, deposit comes back =="
GENERATED=$(swapx rand-preimage 2>/dev/null)
PREIMAGE2=$(printf '%s\n' "$GENERATED" | field preimage)
HASH2=$(printf '%s\n' "$GENERATED" | field payment_hash)
LOCKTIME2=$(( $(height) + 15 ))

INVOICE2=$(swapx ln add-hold --node maker --preimage "$PREIMAGE2" --amount "$AMOUNT" \
  --memo swap 2>/dev/null | field bolt11)
[ -n "$INVOICE2" ] || fail "maker could not create the second hold invoice"

ADDR2=$(swapx htlc-address --payment-hash "$HASH2" --locktime "$LOCKTIME2" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$MAKER_PUBKEY" 2>/dev/null)
read -r FUND_TX2 FUND_VOUT2 FUND_SATS2 <<< "$(fund_htlc "$ADDR2" $(( AMOUNT + CLAIM_FEE )))"
echo "funded:        $FUND_TX2:$FUND_VOUT2 ($FUND_SATS2 sat)"

require_outbound "$AMOUNT" taker

swapx ln pay --node taker --invoice "$INVOICE2" \
  > "$WORK/pay2.out" 2> "$WORK/pay2.err" < /dev/null &
PAY_PID2=$!
STATE=$(await_invoice_state "$HASH2" ACCEPTED maker) \
  || fail "maker never saw the second payment held (state=$STATE)"
echo "maker invoice: $STATE"

# The maker backs out instead of settling. The payer's funds come straight back.
swapx ln cancel --node maker --payment-hash "$HASH2" > /dev/null
STATE=$(await_invoice_state "$HASH2" CANCELED maker) || fail "invoice did not cancel"
echo "maker invoice: $STATE"

wait "$PAY_PID2" || true
[ -z "$(field preimage < "$WORK/pay2.out")" ] \
  || fail "a canceled invoice revealed a preimage"
echo "taker:         payment failed, no preimage revealed"

# The maker's deposit is still locked, and only the timeout releases it.
RAW2=$(swapx refund --refund-key "$MAKER_SECRET" --claim-pubkey "$TAKER_PUBKEY" \
  --payment-hash "$HASH2" --locktime "$LOCKTIME2" \
  --outpoint "$FUND_TX2:$FUND_VOUT2" --value "$FUND_SATS2" \
  --to "$(btc getnewaddress)" --fee 2000 2>/dev/null)
if btc sendrawtransaction "$RAW2" > /dev/null 2>&1; then
  fail "refund accepted before locktime $LOCKTIME2"
fi
echo "refund:        rejected at height $(height), opens at $LOCKTIME2"

while [ "$(height)" -lt "$LOCKTIME2" ]; do mine 1; done
REFUND_TX2=$(btc sendrawtransaction "$RAW2")
mine 1
echo "refund tx:     $REFUND_TX2"
echo "confirmations: $(confirmations "$REFUND_TX2")"
[ "$(confirmations "$REFUND_TX2")" -ge 1 ] || fail "refund did not confirm"

echo
echo "ok: swap settled over Lightning, deposit refunded when the maker backed out"
