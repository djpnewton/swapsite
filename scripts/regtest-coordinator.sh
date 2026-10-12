#!/usr/bin/env bash
#
# The coordinator, end to end, over HTTP.
#
# Drives exactly what the page does: quote, create, check the deposit through the
# relay, pay, claim. The cryptography is the wasm taker; the coordinator only
# brokers.

source "$(dirname "$0")/lib.sh"

AMOUNT="${AMOUNT:-30000}"
WASM="target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
RELAY_URL="http://127.0.0.1:3100"
MAKER_URL="http://127.0.0.1:3200"
COORD_URL="http://127.0.0.1:8090"

SWAPS="$WORK/coord-maker-$$"
RECORDS="$WORK/coord-records-$$"
rm -rf "$SWAPS" "$RECORDS"
mkdir -p "$SWAPS"

cleanup() {
  # Every one of these is allowed to find nothing: a non-zero pkill under
  # `set -e` would abandon the rest of the cleanup, and the exit status with it.
  #
  # The patterns have to match what was actually run: the binaries live at
  # `target/debug/...`, so a pattern of `bin/relay` matches nothing at all.
  pkill -f 'relay --listen 127.0.0.1:3100' 2>/dev/null || true
  pkill -f "maker serve --dir $SWAPS" 2>/dev/null || true
  pkill -f 'coordinator serve --listen 127.0.0.1:8090' 2>/dev/null || true
  return 0
}
trap cleanup EXIT

# Nothing from an earlier run should still be holding a port or a record.
pkill -f 'relay --listen 127.0.0.1:3100' 2>/dev/null || true
pkill -f 'maker serve --dir' 2>/dev/null || true
pkill -f 'coordinator serve --listen 127.0.0.1:8090' 2>/dev/null || true
sleep 0.5

# Read a dotted path out of a JSON document.
jget() {
  python3 -c '
import json, sys
value = json.load(sys.stdin)
for key in sys.argv[1].strip(".").split("."):
    if key:
        value = value[int(key)] if key.isdigit() else value[key]
print(value)
' "$1"
}

echo "== build and start the three services =="
cargo build --quiet --release --target wasm32-unknown-unknown -p swapsite-wasm

"$BIN/relay" --listen 127.0.0.1:3100 > "$WORK/relay.out" 2>&1 &
"$BIN/maker" serve --dir "$SWAPS" --listen 127.0.0.1:3200 > "$WORK/cmaker.out" 2>&1 &
"$BIN/coordinator" serve --listen 127.0.0.1:8090 \
  --maker-url "$MAKER_URL" --relay-url "$RELAY_URL" \
  --dir "$RECORDS" --web web --wasm "$WASM" --dev-pay \
  > "$WORK/coord.out" 2>&1 &

for _ in $(seq 1 30); do
  curl -sf "$RELAY_URL/height" > /dev/null 2>&1 \
    && curl -sf "$MAKER_URL/swaps" > /dev/null 2>&1 \
    && curl -sf "$COORD_URL/config" > /dev/null 2>&1 && break
  sleep 0.5
done
echo "relay:         $RELAY_URL"
echo "maker:         $MAKER_URL"
echo "coordinator:   $COORD_URL"

export SWAPSITE_WASM="$WASM"
export SWAPSITE_RELAY="$RELAY_URL"
taker() { node web/taker-cli.mjs "$@"; }

# --- the page's side -------------------------------------------------------

echo
echo "== the page's key is made in the browser =="
KEY=$(taker keypair)
TAKER_SECRET=$(printf '%s\n' "$KEY" | field secret)
TAKER_PUBKEY=$(printf '%s\n' "$KEY" | field pubkey)
echo "pubkey:        ${TAKER_PUBKEY:0:24}..."

echo
echo "== a quote =="
QUOTE=$(curl -s "$COORD_URL/quote?amount=$AMOUNT")
FEE=$(printf '%s' "$QUOTE" | jget .fee_sat)
INVOICE=$(printf '%s' "$QUOTE" | jget .invoice_sat)
echo "you receive:   $AMOUNT sat"
echo "maker's fee:   $FEE sat"
echo "you pay:       $INVOICE sat over Lightning"

echo
echo "== the coordinator asks the maker for a swap =="
SWAP=$(curl -s -X POST -d "{\"amount\":$AMOUNT,\"claim_pubkey\":\"$TAKER_PUBKEY\"}" "$COORD_URL/swaps")
ID=$(printf '%s' "$SWAP" | jget .id)
HASH=$(printf '%s' "$SWAP" | jget .payment_hash)
LOCKTIME=$(printf '%s' "$SWAP" | jget .locktime)
REFUND_PUBKEY=$(printf '%s' "$SWAP" | jget .refund_pubkey)
CLAIM_FEE=$(printf '%s' "$SWAP" | jget .claim_fee_sat)
SWAP_INVOICE=$(printf '%s' "$SWAP" | jget .invoice_sat)
BOND=$(printf '%s' "$SWAP" | jget .bond_bolt11)
BOND_SAT=$(printf '%s' "$SWAP" | jget .bond_sat)
[ "$SWAP_INVOICE" = "$INVOICE" ] || fail "the swap costs $SWAP_INVOICE but the quote said $INVOICE"
echo "swap id:       $ID"
echo "bond:          $BOND_SAT sat, and nothing is funded until it is paid"

# The coordinator must never be handed the maker's secrets, and a swap that has
# not settled has no preimage to hand over - the field is absent, not null, so
# nothing about a live swap says "the secret is out".
if printf '%s' "$SWAP" | grep -q 'preimage\|secret'; then
  fail "an unsettled swap handed out a secret"
fi
echo "secrets:       none in the swap the page received"

echo
echo "== the taker pays the bond, and the maker funds =="
# Dev pay stands in for the taker's wallet here too. The bond is the first
# thing that wallet pays, and paying it reveals nothing worth having.
BOND_PAID=$(curl -s -X POST "$COORD_URL/dev/pay/$ID" | jget .paid)
[ "$BOND_PAID" = "bond" ] || fail "dev pay did not pay the bond (got '$BOND_PAID')"

DEPOSIT=""
for _ in $(seq 1 40); do
  DEPOSIT=$(curl -s "$COORD_URL/swaps/$ID" | python3 -c '
import json, sys
print(json.load(sys.stdin).get("deposit_txid") or "")
')
  [ -n "$DEPOSIT" ] && break
  sleep 0.5
done
[ -n "$DEPOSIT" ] || fail "the maker never funded the swap after its bond"
echo "deposit:       $DEPOSIT"

mine 1

echo
echo "== the taker checks the deposit through the relay =="
VERIFIED=$(taker verify-deposit --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" --txid "$DEPOSIT")
VOUT=$(printf '%s\n' "$VERIFIED" | field vout)
SATS=$(printf '%s\n' "$VERIFIED" | field sats)
echo "htlc:          $(printf '%s\n' "$VERIFIED" | field address)"
echo "deposit:       $DEPOSIT:$VOUT ($SATS sat, $(printf '%s\n' "$VERIFIED" | field confirmations) conf)"

echo
echo "== pay, and the maker settles =="
PREIMAGE=$(curl -s -X POST "$COORD_URL/dev/pay/$ID" | jget .preimage)
[ -n "$PREIMAGE" ] || fail "no preimage came back"
echo "preimage:      ${PREIMAGE:0:16}... (dev pay stands in for the taker's wallet)"

STATE=$(curl -s "$COORD_URL/swaps/$ID" | jget .state)
[ "$STATE" = "settled" ] || fail "the maker did not settle (state=$STATE)"
echo "swap state:    $STATE"

# Settling is the reveal, so the maker publishes the preimage from that moment.
# That is what lets a taker who paid from another wallet claim without carrying
# the secret across by hand.
PUBLISHED=$(curl -s "$COORD_URL/swaps/$ID" | jget .preimage)
[ "$PUBLISHED" = "$PREIMAGE" ] || fail "a settled swap withheld its preimage (got '$PUBLISHED')"
echo "revealed:      ${PUBLISHED:0:16}... (published by the maker once it settled)"

echo
echo "== the taker claims on chain =="
JOINED=$(taker check-join --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" --preimage "$PREIMAGE")
echo "join:          $(printf '%s\n' "$JOINED" | field joined)"

DEST=$(btc getnewaddress)
CLAIM=$(taker claim --claim-key "$TAKER_SECRET" --payment-hash "$HASH" \
  --locktime "$LOCKTIME" --refund-pubkey "$REFUND_PUBKEY" --preimage "$PREIMAGE" \
  --outpoint "$DEPOSIT:$VOUT" --value "$SATS" --to "$DEST" --fee "$CLAIM_FEE")
CLAIM_TX=$(taker broadcast --tx-hex "$(printf '%s\n' "$CLAIM" | field tx_hex)" | field txid)
mine 1
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || fail "the claim did not confirm"

read -r _ RECEIVED <<< "$(find_output "$CLAIM_TX" "$DEST")"
[ "$RECEIVED" = "$AMOUNT" ] || fail "taker received $RECEIVED, expected $AMOUNT"
echo "claim tx:      $CLAIM_TX"
echo "received:      $RECEIVED sat on chain"

echo
echo "== a refreshed page finds the swap again =="
RECOVERED=$(curl -s "$COORD_URL/swaps?claim_pubkey=$TAKER_PUBKEY")
FOUND=$(printf '%s' "$RECOVERED" | python3 -c '
import json, sys
swaps = json.load(sys.stdin)["swaps"]
print(",".join(s["id"] for s in swaps))
')
case ",$FOUND," in
  *",$ID,"*) echo "recovered:     $ID" ;;
  *) fail "the coordinator did not remember the swap (got $FOUND)" ;;
esac

OTHER=$(curl -s "$COORD_URL/swaps?claim_pubkey=02$(printf '%062d' 0)")
COUNT=$(printf '%s' "$OTHER" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["swaps"]))')
[ "$COUNT" = "0" ] || fail "another taker's key returned $COUNT swaps"
echo "isolation:     another key sees $COUNT swaps"

echo
echo "ok: quoted, brokered, verified, paid, settled, claimed, and recovered"
