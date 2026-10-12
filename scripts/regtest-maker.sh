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
  #
  # A non-zero pkill under `set -e` would take the exit status with it.
  pkill -f "maker run --dir $SWAPS" 2>/dev/null || true
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
  --claim-pubkey "$TAKER_PUBKEY" 2>/dev/null)
ID=$(printf '%s\n' "$NEW" | field id)
BOLT11=$(printf '%s\n' "$NEW" | field bolt11)
BOND=$(printf '%s\n' "$NEW" | field bond_bolt11)
BOND_SAT=$(printf '%s\n' "$NEW" | field bond_sat)
BOND_BY=$(printf '%s\n' "$NEW" | field bond_by)
ADDRESS=$(printf '%s\n' "$NEW" | field htlc_address)
HASH=$(printf '%s\n' "$NEW" | field payment_hash)
LOCKTIME=$(printf '%s\n' "$NEW" | field locktime)
REFUND_PUBKEY=$(printf '%s\n' "$NEW" | field refund_pubkey)
SWAP_AMOUNT=$(printf '%s\n' "$NEW" | field amount)
CLAIM_FEE=$(printf '%s\n' "$NEW" | field claim_fee)

echo "swap id:       $ID"
echo "amount:        $SWAP_AMOUNT sat over Lightning"
echo "deposit:       not funded yet: the bond comes first"
echo "bond:          $BOND_SAT sat, due by height $BOND_BY"
echo "refund after:  height $LOCKTIME"

# --- funding, then a cold start -------------------------------------------
#
# The daemon has never seen this swap, so it has to fund it from the record
# alone - and it will not fund it until the bond is in.

echo
echo "== starting the daemon so it can fund the swap =="
maker run --dir "$SWAPS" --interval 1 > "$WORK/maker.out" 2> "$WORK/maker.err" &

echo
echo "== taker pays the bond, and the daemon funds =="
require_outbound "$(( SWAP_AMOUNT + BOND_SAT ))" taker
read -r DEPOSIT DEPOSIT_VOUT <<< "$(bond_and_fund "$SWAPS" "$ID" "$BOND")"
echo "deposit:       $DEPOSIT:$DEPOSIT_VOUT ($(( SWAP_AMOUNT + CLAIM_FEE )) sat on chain)"

mine 1 # confirm the deposit

# Away again, so the payment below is left held rather than settled on the spot.
pkill -f "maker run --dir $SWAPS" 2>/dev/null || true
sleep 0.5

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
echo "== the daemon comes back to a funded swap with the payment held =="
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
echo "== a second swap: the taker pays the bond and then walks away =="
NEW2=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" 2>/dev/null)
ID2=$(printf '%s\n' "$NEW2" | field id)
LOCKTIME2=$(printf '%s\n' "$NEW2" | field locktime)
BOND2=$(printf '%s\n' "$NEW2" | field bond_bolt11)
BOND2_SAT=$(printf '%s\n' "$NEW2" | field bond_sat)
echo "swap id:       $ID2"
echo "refund after:  height $LOCKTIME2"

# The bond is what the maker keeps for its trouble, and it is paid before any
# of its own capital moves.
read -r DEPOSIT2 _ <<< "$(bond_and_fund "$SWAPS" "$ID2" "$BOND2")"
echo "bond:          $BOND2_SAT sat collected, and the taker stopped there"
echo "deposit:       $DEPOSIT2"

mine 1
STATE=$(await_swap_state "$ID2" initiator_funded "$SWAPS") \
  || fail "the daemon did not record the deposit (state=$STATE)"
echo "swap state:    $STATE"

STATE=$(maker status --dir "$SWAPS" --id "$ID2" | field state)
[ "$STATE" = "initiator_funded" ] \
  || fail "the daemon acted before the timeout (state=$STATE)"
echo "before timeout: still $STATE, deposit still locked"

while [ "$(height)" -lt "$LOCKTIME2" ]; do mine "$(( LOCKTIME2 - $(height) ))"; done

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

# --- a refund the daemon did not build -------------------------------------
#
# The record only ever holds the refund this daemon built, so a refund broadcast
# from anywhere else used to leave the swap unfinished for good: the deposit was
# gone, the timeout had passed, and every tick rebuilt and rebroadcast a
# transaction that could never be mined.

pkill -f "maker run --dir $SWAPS" 2>/dev/null || true
sleep 0.5

echo
echo "== a third swap, refunded by someone else while the daemon is down =="
NEW3=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" 2>/dev/null)
ID3=$(printf '%s\n' "$NEW3" | field id)
HASH3=$(printf '%s\n' "$NEW3" | field payment_hash)
ADDRESS3=$(printf '%s\n' "$NEW3" | field htlc_address)
LOCKTIME3=$(printf '%s\n' "$NEW3" | field locktime)
BOND3=$(printf '%s\n' "$NEW3" | field bond_bolt11)
echo "swap id:       $ID3"
echo "refund after:  height $LOCKTIME3"
echo "paid for:      no, so nothing but a refund can spend the deposit"

# Bring the daemon back just long enough to fund it, then take it away again:
# the refund that follows is not this daemon's own.
maker run --dir "$SWAPS" --interval 1 > "$WORK/maker3a.out" 2> "$WORK/maker3a.err" &
DEPOSIT3=$(bond_and_fund "$SWAPS" "$ID3" "$BOND3" | cut -d' ' -f1)
mine 1
pkill -f "maker run --dir $SWAPS" 2>/dev/null || true
sleep 0.5

read -r VOUT3 SATS3 <<< "$(find_output "$DEPOSIT3" "$ADDRESS3")"
[ -n "$VOUT3" ] || fail "the deposit does not pay the HTLC"

# Whichever process holds this key, it is in the record.
REFUND_SECRET3=$(python3 -c '
import json, sys
print(json.load(open(sys.argv[1]))["refund_secret"])
' "$SWAPS/$ID3.json")

while [ "$(height)" -lt "$LOCKTIME3" ]; do mine "$(( LOCKTIME3 - $(height) ))"; done
RAW3=$(swapx refund --payment-hash "$HASH3" --locktime "$LOCKTIME3" \
  --claim-pubkey "$TAKER_PUBKEY" --refund-key "$REFUND_SECRET3" \
  --outpoint "$DEPOSIT3:$VOUT3" --value "$SATS3" \
  --to "$(btc getnewaddress)" 2>/dev/null)
TX3=$(btc sendrawtransaction "$RAW3")
mine 1
echo "refunded by:   someone other than the daemon ($TX3)"

echo
echo "== the daemon starts and finds the deposit already gone =="
maker run --dir "$SWAPS" --interval 1 > "$WORK/maker3.out" 2> "$WORK/maker3.err" &
STATE=$(await_swap_state "$ID3" refunded "$SWAPS") \
  || fail "the daemon never noticed the deposit was gone (state=$STATE)"
echo "swap state:    $STATE"

# --- timeouts that cannot be safe -----------------------------------------
#
# The refund has to outlive the Lightning payment the taker makes. A margin that
# does not would let the maker refund the deposit and still settle the payment,
# so the swap is refused before anything is offered or funded.

echo
echo "== a refund that would open inside the payment's lifetime is refused =="
if maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
    --claim-pubkey "$TAKER_PUBKEY" --locktime-margin 30 \
    > /dev/null 2> "$WORK/bad-margin.err"; then
  fail "the maker offered a swap whose refund opens inside the payment's life"
fi
grep -q "refusing to offer this swap" "$WORK/bad-margin.err" \
  || fail "expected the maker to refuse the margin, got: $(cat "$WORK/bad-margin.err")"
echo "refused:       $(cut -c1-72 < "$WORK/bad-margin.err")"

# The smallest margin the protocol allows is enough, so the floor is a floor.
echo
NEW4=$(maker new-swap --dir "$SWAPS" --amount "$AMOUNT" \
  --claim-pubkey "$TAKER_PUBKEY" --locktime-margin 52 2>/dev/null)
ID4=$(printf '%s\n' "$NEW4" | field id)
[ -n "$ID4" ] || fail "a margin of exactly 52 was refused"
echo "accepted:      a margin of 52 blocks (40 CLTV + 12)"
pkill -f "maker run --dir $SWAPS" 2>/dev/null || true

# It has to adopt what happened rather than act on it: a refund of its own here
# would be a transaction it could never get mined.
OWN=$(python3 -c '
import json, sys
print(json.load(open(sys.argv[1]))["refund_tx"] or "none")
' "$SWAPS/$ID3.json")
[ "$OWN" = "none" ] || fail "the daemon built a refund for an already-refunded deposit"
echo "own refund:    none, so nothing is being rebroadcast forever"

echo
echo "ok: settled a swap it never saw created, refunded one that timed out,"
echo "    and adopted a refund it did not send"
