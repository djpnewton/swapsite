#!/usr/bin/env bash
#
# The maker daemon, end to end.
#
# Runs the whole maker side without a human in the loop: a daemon started cold
# picks up a swap it never saw created and settles it once the taker pays, and
# it refunds a deposit whose taker never paid.

source "$(dirname "$0")/lib.sh"

AMOUNT="${AMOUNT:-30000}"
SWAPS="$WORK/maker-swaps-$$"
rm -rf "$SWAPS"
mkdir -p "$SWAPS"

cleanup() {
  # Match on the store directory rather than trusting a pid: a missed daemon
  # keeps rewriting its records and makes the next run lie.
  pkill -f "maker run --dir $SWAPS" 2>/dev/null
  return 0
}
trap cleanup EXIT

# No daemon should outlive a test run: a stray one keeps rewriting the swap
# records it knows about, which is exactly what a fresh run must not see.
pkill -f 'maker run --dir' 2>/dev/null || true

# Only the taker has this secret.
TAKER=$(swapx keygen)
TAKER_SECRET=$(printf '%s\n' "$TAKER" | field secret)
TAKER_PUBKEY=$(printf '%s\n' "$TAKER" | field pubkey)

# --- a swap created while the daemon is down -------------------------------

echo "== the daemon is not running =="
NEW=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" --locktime-margin 30 2>/dev/null)
ID=$(printf '%s\n' "$NEW" | field id)
BOLT11=$(printf '%s\n' "$NEW" | field bolt11)
ADDRESS=$(printf '%s\n' "$NEW" | field htlc_address)
HASH=$(printf '%s\n' "$NEW" | field payment_hash)
DEPOSIT=$(printf '%s\n' "$NEW" | field deposit_txid)
LOCKTIME=$(printf '%s\n' "$NEW" | field locktime)
REFUND_PUBKEY=$(printf '%s\n' "$NEW" | field refund_pubkey)
SWAP_AMOUNT=$(printf '%s\n' "$NEW" | field amount)
CLAIM_FEE=$(printf '%s\n' "$NEW" | field claim_fee)

echo "swap id:       $ID"
echo "amount:        $SWAP_AMOUNT sat over Lightning"
echo "deposit:       $DEPOSIT ($(( SWAP_AMOUNT + CLAIM_FEE )) sat on chain)"
echo "refund after:  height $LOCKTIME"

mine 1 # confirm the deposit

echo
echo "== taker checks the on-chain leg from public values only =="
EXPECTED=$(swapx htlc-address --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" 2>/dev/null)
[ "$EXPECTED" = "$ADDRESS" ] || fail "htlc address mismatch: $EXPECTED vs $ADDRESS"
read -r DEPOSIT_VOUT DEPOSIT_SATS <<< "$(find_output "$DEPOSIT" "$EXPECTED")"
[ -n "$DEPOSIT_VOUT" ] || fail "the deposit does not pay the HTLC"
echo "matches:       $ADDRESS ($DEPOSIT_SATS sat)"

echo
echo "== taker pays, with the daemon still down =="
require_outbound "$SWAP_AMOUNT" taker
swapx ln pay --node taker --invoice "$BOLT11" \
  > "$WORK/maker-pay.out" 2> "$WORK/maker-pay.err" < /dev/null &
PAY_PID=$!

STATE=$(await_invoice_state "$HASH" ACCEPTED maker) \
  || fail "maker never saw the payment held (state=$STATE)"
echo "invoice:       $STATE, and nothing is settling it yet"

# --- cold start ------------------------------------------------------------

echo
echo "== starting the daemon on a swap it never saw created =="
maker run --dir "$SWAPS" --interval 1 > "$WORK/maker.out" 2> "$WORK/maker.err" &

STATE=$(await_swap_state "$ID" settled "$SWAPS") \
  || fail "the daemon did not settle the swap (state=$STATE)"
echo "swap state:    $STATE"
grep -o 'settled, preimage revealed to the taker' "$WORK/maker.err" | tail -1

wait "$PAY_PID" || fail "taker payment failed"
REVEALED=$(field preimage < "$WORK/maker-pay.out")
[ -n "$REVEALED" ] || fail "taker learned no preimage"

echo
echo "== taker claims the deposit =="
swapx check-join --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" \
  --preimage "$REVEALED" > /dev/null

DEST=$(btc getnewaddress)
RAW=$(swapx claim --claim-key "$TAKER_SECRET" --refund-pubkey "$REFUND_PUBKEY" \
  --payment-hash "$HASH" --locktime "$LOCKTIME" --preimage "$REVEALED" \
  --outpoint "$DEPOSIT:$DEPOSIT_VOUT" --value "$DEPOSIT_SATS" --to "$DEST" \
  --fee "$CLAIM_FEE" 2>/dev/null)
CLAIM_TX=$(btc sendrawtransaction "$RAW")
mine 1
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || fail "claim did not confirm"

read -r _ RECEIVED <<< "$(find_output "$CLAIM_TX" "$DEST")"
[ "$RECEIVED" = "$SWAP_AMOUNT" ] || fail "taker received $RECEIVED, expected $SWAP_AMOUNT"
echo "swap done:     $SWAP_AMOUNT sat moved Lightning -> on-chain"

# --- the taker never pays --------------------------------------------------

echo
echo "== a second swap, whose taker never pays =="
NEW2=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" --locktime-margin 8 2>/dev/null)
ID2=$(printf '%s\n' "$NEW2" | field id)
LOCKTIME2=$(printf '%s\n' "$NEW2" | field locktime)
DEPOSIT2=$(printf '%s\n' "$NEW2" | field deposit_txid)
echo "swap id:       $ID2"
echo "deposit:       $DEPOSIT2"
echo "refund after:  height $LOCKTIME2"

mine 1
STATE=$(await_swap_state "$ID2" initiator_funded "$SWAPS") \
  || fail "the daemon did not record the deposit (state=$STATE)"
echo "swap state:    $STATE"

STATE=$(maker status --dir "$SWAPS" --id "$ID2" | field state)
[ "$STATE" = "initiator_funded" ] \
  || fail "the daemon acted before the timeout (state=$STATE)"
echo "before timeout: still $STATE, deposit still locked"

while [ "$(height)" -lt "$LOCKTIME2" ]; do mine 1; done

# The daemon builds and broadcasts the refund as soon as the timeout opens. The
# swap only reads as `refunded` once that transaction is actually in a block, so
# wait for the broadcast first, then mine.
REFUND_TXID="-"
for _ in $(seq 1 20); do
  REFUND_TXID=$(maker status --dir "$SWAPS" --id "$ID2" | field refund_txid) || true
  [ "$REFUND_TXID" != "-" ] && break
  sleep 1
done
[ "$REFUND_TXID" != "-" ] || fail "the daemon built no refund after the timeout"
echo "refund tx:     $REFUND_TXID (broadcast)"

mine 1
STATE=$(await_swap_state "$ID2" refunded "$SWAPS") \
  || fail "the swap never read as refunded (state=$STATE)"
echo "swap state:    $STATE"
[ "$(confirmations "$REFUND_TXID")" -ge 1 ] || fail "refund did not confirm"

echo
echo "ok: daemon settled a swap it never saw created, and refunded one that timed out"
