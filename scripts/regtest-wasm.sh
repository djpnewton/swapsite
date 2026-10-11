#!/usr/bin/env bash
#
# The browser-side taker, end to end.
#
# A taker with no node, no docker and no wallet software: everything
# cryptographic runs in the wasm module, and a relay is used for two things
# only, reading the chain and broadcasting. The taker checks the deposit itself
# and signs its own claim.

source "$(dirname "$0")/lib.sh"

WASM="target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
RELAY_URL="http://127.0.0.1:3100"
AMOUNT="${AMOUNT:-30000}"
SWAPS="$WORK/wasm-swaps-$$"
rm -rf "$SWAPS"
mkdir -p "$SWAPS"

RELAY_PID=""
cleanup() {
  [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
  pkill -f "maker run --dir $SWAPS" 2>/dev/null
  return 0
}
trap cleanup EXIT

pkill -f 'maker run --dir' 2>/dev/null || true

echo "== build the wasm module =="
cargo build --quiet --release --target wasm32-unknown-unknown -p swapsite-wasm
echo "module:        $WASM ($(du -h "$WASM" | cut -f1))"

export SWAPSITE_WASM="$WASM"
export SWAPSITE_RELAY="$RELAY_URL"
taker() { node web/taker-cli.mjs "$@"; }

echo
echo "== start the relay =="
"$BIN/relay" --listen 127.0.0.1:3100 > "$WORK/relay.out" 2> "$WORK/relay.err" &
RELAY_PID=$!
for _ in $(seq 1 30); do
  taker height > /dev/null 2>&1 && break
  sleep 0.5
done
echo "relay:         $RELAY_URL (height $(taker height | field height), chain height $(height))"

# --- the taker, in the browser's own wasm ----------------------------------

echo
echo "== the taker makes its claim key in wasm =="
KEY=$(taker keypair)
TAKER_SECRET=$(printf '%s\n' "$KEY" | field secret)
TAKER_PUBKEY=$(printf '%s\n' "$KEY" | field pubkey)
echo "pubkey:        ${TAKER_PUBKEY:0:24}..."

echo
echo "== maker creates the swap =="
NEW=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" --locktime-margin 30 2>/dev/null)
ID=$(printf '%s\n' "$NEW" | field id)
BOLT11=$(printf '%s\n' "$NEW" | field bolt11)
HASH=$(printf '%s\n' "$NEW" | field payment_hash)
DEPOSIT=$(printf '%s\n' "$NEW" | field deposit_txid)
LOCKTIME=$(printf '%s\n' "$NEW" | field locktime)
REFUND_PUBKEY=$(printf '%s\n' "$NEW" | field refund_pubkey)
SWAP_AMOUNT=$(printf '%s\n' "$NEW" | field amount)
CLAIM_FEE=$(printf '%s\n' "$NEW" | field claim_fee)
mine 1
echo "swap id:       $ID"
echo "amount:        $SWAP_AMOUNT sat over Lightning"

echo
echo "== taker checks the deposit through the relay =="
VERIFIED=$(taker verify-deposit --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" --txid "$DEPOSIT")
ADDRESS=$(printf '%s\n' "$VERIFIED" | field address)
VOUT=$(printf '%s\n' "$VERIFIED" | field vout)
SATS=$(printf '%s\n' "$VERIFIED" | field sats)
CONFS=$(printf '%s\n' "$VERIFIED" | field confirmations)
echo "htlc address:  $ADDRESS"
echo "deposit:       $DEPOSIT:$VOUT ($SATS sat, $CONFS conf)"
echo "redeem script: $(printf '%s\n' "$VERIFIED" | field redeem_script | cut -c1-40)..."

echo
echo "== the taker refuses a deposit for a different hash =="
if taker verify-deposit --payment-hash "11${HASH:2}" --locktime "$LOCKTIME" \
    --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" \
    --txid "$DEPOSIT" > /dev/null 2>&1; then
  fail "the taker accepted a deposit that pays a different HTLC"
fi
echo "rejected:      a deposit that does not pay this HTLC"

# --- settle, then claim from the browser side ------------------------------

echo
echo "== the maker settles once the taker pays =="
maker run --dir "$SWAPS" --interval 1 > "$WORK/maker.out" 2> "$WORK/maker.err" &

require_outbound "$SWAP_AMOUNT" taker
swapx ln pay --node taker --invoice "$BOLT11" \
  > "$WORK/pay.out" 2> "$WORK/pay.err" < /dev/null &
PAY_PID=$!

STATE=$(await_swap_state "$ID" settled "$SWAPS") \
  || fail "the daemon did not settle the swap (state=$STATE)"
wait "$PAY_PID" || fail "taker payment failed"
PREIMAGE=$(field preimage < "$WORK/pay.out")
[ -n "$PREIMAGE" ] || fail "no preimage was revealed"
echo "swap state:    $STATE, preimage revealed"

echo
echo "== taker claims from the browser side =="
JOINED=$(taker check-join --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" --preimage "$PREIMAGE")
echo "join:          $(printf '%s\n' "$JOINED" | field joined)"

DEST=$(btc getnewaddress)
CLAIM=$(taker claim --claim-key "$TAKER_SECRET" --payment-hash "$HASH" \
  --locktime "$LOCKTIME" --refund-pubkey "$REFUND_PUBKEY" --preimage "$PREIMAGE" \
  --outpoint "$DEPOSIT:$VOUT" --value "$SATS" --to "$DEST" --fee "$CLAIM_FEE")
CLAIM_HEX=$(printf '%s\n' "$CLAIM" | field tx_hex)

CLAIM_TX=$(taker broadcast --tx-hex "$CLAIM_HEX" | field txid)
mine 1
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || fail "the claim did not confirm"

read -r _ RECEIVED <<< "$(find_output "$CLAIM_TX" "$DEST")"
[ "$RECEIVED" = "$SWAP_AMOUNT" ] || fail "taker received $RECEIVED, expected $SWAP_AMOUNT"

echo "claim tx:      $CLAIM_TX (broadcast through the relay)"
echo "confirmations: $(confirmations "$CLAIM_TX")"
echo
echo "ok: the wasm taker verified the deposit, claimed it, and ran no node"
