#!/usr/bin/env bash
#
# A btc2ln swap end to end: the taker deposits on chain first, the maker pays
# the taker's invoice over Lightning, and the maker claims the deposit with the
# preimage that payment revealed.
#
# The taker here is ordinary tooling (lncli, swapx, bitcoin-cli) and no bond is
# posted, which is the point of this direction: the deposit is the collateral,
# and a taker that walks away has parked its own money until the locktime.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

STORE="$WORK/btc2ln-store"
rm -rf "$STORE"

FEE=100
CLAIM_FEE=2000

lncli() {
  docker compose -f docker/docker-compose.yml exec -T "lnd-$1" \
    lncli --network=regtest "${@:2}"
}

# The taker's half of one swap: a secret, and an invoice that pays to it.
# Echoes "<preimage> <payment_hash> <bolt11>".
taker_terms() {
  local sats="$1" out preimage hash bolt
  out=$(swapx rand-preimage)
  preimage=$(printf '%s\n' "$out" | field preimage)
  hash=$(printf '%s\n' "$out" | field payment_hash)
  bolt=$(lncli taker addinvoice --amt "$sats" --preimage "$preimage" \
    --memo "btc2ln swap" \
    | grep -oE '"payment_request" *: *"[^"]*"' | head -1 | sed 's/.*: *"//;s/"$//')
  [ -n "$bolt" ] || fail "the taker's node produced no invoice"
  echo "$preimage $hash $bolt"
}

# Ask the maker for an offer and read back what the taker has to do.
# Sets OFFER, ID, DEPOSIT, ADDRESS and CLAIM_PUBKEY.
offer() {
  local locktime="$1" bolt="$2" refund_pubkey="$3"
  OFFER=$(maker btc2ln-offer --dir "$STORE" --invoice "$bolt" \
    --refund-pubkey "$refund_pubkey" --locktime "$locktime" \
    --fee "$FEE" --claim-fee "$CLAIM_FEE") || return 1
  ID=$(printf '%s\n' "$OFFER" | field id)
  DEPOSIT=$(printf '%s\n' "$OFFER" | field deposit_sat)
  ADDRESS=$(printf '%s\n' "$OFFER" | field htlc_address)
  CLAIM_PUBKEY=$(printf '%s\n' "$OFFER" | field claim_pubkey)
}

KEYS=$(swapx keygen)
REFUND_KEY=$(printf '%s\n' "$KEYS" | field secret)
REFUND_PUBKEY=$(printf '%s\n' "$KEYS" | field pubkey)

echo "== the taker's terms become the maker's offer =="
SATS=20000
# This direction is the maker paying, so it is the maker's outbound capacity
# that has to cover the invoice.
require_outbound "$SATS" maker
read -r PREIMAGE HASH BOLT <<< "$(taker_terms "$SATS")"
# Far enough out that any plausible CLTV delta, window and margin fit inside
# it: the maker is the one that knows the invoice's own delta.
LOCKTIME=$(( $(height) + 200 ))
offer "$LOCKTIME" "$BOLT" "$REFUND_PUBKEY" || fail "the maker refused a good locktime"
[ "$ID" = "$HASH" ] || fail "the maker built the swap on hash $ID, not $HASH"
[ "$DEPOSIT" = "$(( SATS + FEE + CLAIM_FEE ))" ] \
  || fail "the deposit is $DEPOSIT, expected the invoice plus both fees"
echo "offer:   $DEPOSIT sat for $SATS over Lightning, refund at $LOCKTIME"

# The taker does not take the maker's word for the address: it rebuilds the
# HTLC from the terms and checks that they describe the same output.
REBUILT=$(swapx htlc-address --payment-hash "$HASH" \
  --claim-pubkey "$CLAIM_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" \
  --locktime "$LOCKTIME")
REBUILT=${REBUILT#address=}
[ "$REBUILT" = "$ADDRESS" ] \
  || fail "the maker's address is not the HTLC the terms describe"
echo "address: the taker's own HTLC, funded by the taker alone"

echo "== the deposit is funded, confirmed and paid for =="
read -r TXID VOUT _ <<< "$(fund_htlc "$ADDRESS" "$DEPOSIT")"
maker btc2ln-funded --dir "$STORE" --id "$ID" --outpoint "$TXID:$VOUT" > /dev/null
mine 1

# One sweep pays, the next claims: the tick plans from what it knew, and the
# preimage only exists once the payment has settled.
maker btc2ln-run --dir "$STORE" --once
maker btc2ln-run --dir "$STORE" --once

STATE=$(swapx ln state --node taker --payment-hash "$HASH" | field state)
[ "$STATE" = "SETTLED" ] || fail "the taker's invoice is $STATE, not SETTLED"
echo "invoice: the maker paid it, revealing the preimage"

CLAIM_TXID=$(maker btc2ln-status --dir "$STORE" --id "$ID" | field claim_txid)
[ "$CLAIM_TXID" != "-" ] || fail "the maker never claimed the deposit"
mine 1
[ -z "$(btc gettxout "$TXID" "$VOUT" 2>/dev/null)" ] \
  || fail "the deposit is still unspent after the claim confirmed"

GOT=$(btc getrawtransaction "$CLAIM_TXID" true | python3 -c '
import json, sys
tx = json.load(sys.stdin)
print(round(sum(o["value"] for o in tx["vout"]) * 100000000))
')
[ "$GOT" = "$(( DEPOSIT - CLAIM_FEE ))" ] \
  || fail "the claim paid out $GOT, expected the deposit less its fee"
echo "claim:   $GOT sat, the deposit less the claim fee"

echo "== a locktime that leaves no room is refused =="
read -r _ SHORT_HASH SHORT_BOLT <<< "$(taker_terms "$SATS")"
SHORT=$(( $(height) + 20 ))
REASON=$(offer "$SHORT" "$SHORT_BOLT" "$REFUND_PUBKEY" 2>&1 > /dev/null || true)
printf '%s\n' "$REASON" | grep -q "refusing to offer this swap" \
  || fail "a 20 block locktime was not refused: $REASON"
echo "refused: a payment made in the next 30 blocks could not be claimed"

echo "== a deposit that arrives too late is never paid for =="
read -r _ LATE_HASH LATE_BOLT <<< "$(taker_terms "$SATS")"
LATE_LOCKTIME=$(( $(height) + 130 ))
offer "$LATE_LOCKTIME" "$LATE_BOLT" "$REFUND_PUBKEY" || fail "the maker refused a good locktime"
LATE_ID=$ID LATE_DEPOSIT=$DEPOSIT LATE_ADDRESS=$ADDRESS

# The taker dawdles until a payment would expire within the margin of the
# refund, then funds anyway. The deposit is valid; the payment would not be.
mine 60
read -r LATE_TXID LATE_VOUT _ <<< "$(fund_htlc "$LATE_ADDRESS" "$LATE_DEPOSIT")"
maker btc2ln-funded --dir "$STORE" --id "$LATE_ID" \
  --outpoint "$LATE_TXID:$LATE_VOUT" > /dev/null
mine 1

SWEEP=$(maker btc2ln-run --dir "$STORE" --once 2>&1)
printf '%s\n' "$SWEEP" | grep -q "too_late_to_pay" \
  || fail "a late deposit was not refused: $SWEEP"
LATE_STATE=$(swapx ln state --node taker --payment-hash "$LATE_HASH" | field state)
[ "$LATE_STATE" = "OPEN" ] \
  || fail "the maker paid an invoice it had said it would not pay: $LATE_STATE"
echo "refused: the invoice is still $LATE_STATE and nothing was paid"

echo "== the taker takes its deposit back =="
mine "$(( LATE_LOCKTIME - $(height) + 1 ))"
REFUND=$(swapx refund --payment-hash "$LATE_HASH" --locktime "$LATE_LOCKTIME" \
  --claim-pubkey "$CLAIM_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" \
  --refund-key "$REFUND_KEY" --outpoint "$LATE_TXID:$LATE_VOUT" \
  --value "$LATE_DEPOSIT" --to "$(btc getnewaddress)" --fee 1000)
REFUND_TXID=$(btc sendrawtransaction "$REFUND")
mine 1
[ -n "$(confirmations "$REFUND_TXID")" ] || fail "the refund did not confirm"
[ -z "$(btc gettxout "$LATE_TXID" "$LATE_VOUT" 2>/dev/null)" ] \
  || fail "the refused deposit is still unspent after the refund"
echo "refund:  the deposit is back in the taker's wallet"

echo "ok: btc2ln swap funded, paid, claimed, and an unsafe one refused"
