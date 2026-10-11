#!/usr/bin/env bash
#
# End-to-end test of one on-chain HTLC leg against the local regtest chain.
#
# Funds an HTLC and claims it with the preimage, then funds a second HTLC and
# refunds it after the timeout. Also checks that the refund is rejected early,
# which is the invariant the whole swap depends on.
#
# Requires: docker compose stack up (docs/regtest.md), cargo, python3.

source "$(dirname "$0")/lib.sh"

# --- claim path ------------------------------------------------------------

echo "== claim path =="
CLAIMANT=$(swapx keygen)
CLAIM_SECRET=$(printf '%s\n' "$CLAIMANT" | field secret)
CLAIM_PUBKEY=$(printf '%s\n' "$CLAIMANT" | field pubkey)
REFUNDER=$(swapx keygen)
REFUND_PUBKEY=$(printf '%s\n' "$REFUNDER" | field pubkey)

GENERATED=$(swapx rand-preimage 2>/dev/null)
PREIMAGE=$(printf '%s\n' "$GENERATED" | field preimage)
HASH=$(printf '%s\n' "$GENERATED" | field payment_hash)

LOCKTIME=$(( $(height) + 20 ))
ADDR=$(swapx htlc-address --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$CLAIM_PUBKEY" --refund-pubkey "$REFUND_PUBKEY" 2>/dev/null)

echo "htlc address:  $ADDR"
echo "refund height: $LOCKTIME"

read -r TXID VOUT SATS <<< "$(fund_htlc "$ADDR" 100000000)"
echo "funded:        $TXID:$VOUT ($SATS sat)"

DEST=$(btc getnewaddress)
RAW=$(swapx claim --claim-key "$CLAIM_SECRET" --refund-pubkey "$REFUND_PUBKEY" \
  --payment-hash "$HASH" --locktime "$LOCKTIME" --preimage "$PREIMAGE" \
  --outpoint "$TXID:$VOUT" --value "$SATS" --to "$DEST" --fee 2000 2>/dev/null)
CLAIM_TX=$(btc sendrawtransaction "$RAW")
mine 1

echo "claim tx:      $CLAIM_TX"
echo "confirmations: $(confirmations "$CLAIM_TX")"
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || fail "claim did not confirm"

# --- refund path -----------------------------------------------------------

echo
echo "== refund path =="
CLAIMANT2=$(swapx keygen)
CLAIM_PUBKEY2=$(printf '%s\n' "$CLAIMANT2" | field pubkey)
REFUND_SECRET=$(printf '%s\n' "$REFUNDER" | field secret)

LOCKTIME=$(( $(height) + 12 ))
ADDR=$(swapx htlc-address --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --claim-pubkey "$CLAIM_PUBKEY2" --refund-pubkey "$REFUND_PUBKEY" 2>/dev/null)
echo "htlc address:  $ADDR"
echo "refund height: $LOCKTIME"

read -r TXID VOUT SATS <<< "$(fund_htlc "$ADDR" 100000000)"
echo "funded:        $TXID:$VOUT ($SATS sat)"

DEST=$(btc getnewaddress)
RAW=$(swapx refund --refund-key "$REFUND_SECRET" --claim-pubkey "$CLAIM_PUBKEY2" \
  --payment-hash "$HASH" --locktime "$LOCKTIME" \
  --outpoint "$TXID:$VOUT" --value "$SATS" --to "$DEST" --fee 2000 2>/dev/null)

# The refund must be useless before the timeout opens.
if btc sendrawtransaction "$RAW" > /dev/null 2>&1; then
  fail "refund accepted at height $(height), before locktime $LOCKTIME"
fi
echo "early refund:  correctly rejected at height $(height)"

while [ "$(height)" -lt "$LOCKTIME" ]; do mine 1; done

REFUND_TX=$(btc sendrawtransaction "$RAW")
mine 1

echo "refund tx:     $REFUND_TX"
echo "confirmations: $(confirmations "$REFUND_TX")"
[ "$(confirmations "$REFUND_TX")" -ge 1 ] || fail "refund did not confirm"

echo
echo "ok: claim and refund both confirmed on regtest"
