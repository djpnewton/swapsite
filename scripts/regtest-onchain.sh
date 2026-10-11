#!/usr/bin/env bash
#
# End-to-end test of one on-chain HTLC leg against the local regtest chain.
#
# Funds an HTLC and claims it with the preimage, then funds a second HTLC and
# refunds it after the timeout. Also checks that the refund is rejected early,
# which is the invariant the whole swap depends on.
#
# Requires: docker compose stack up (docs/regtest.md), cargo, awk.

set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -f docker/docker-compose.yml)
BTC=("${COMPOSE[@]}" exec -T bitcoind bitcoin-cli -regtest \
  -rpcuser=bitcoin -rpcpassword=bitcoin -rpcwallet=swap)
SWAPX=(cargo run --quiet --bin swapx --)

WORK="${TMPDIR:-/tmp}/swapsite-regtest"
mkdir -p "$WORK"

# --- helpers ---------------------------------------------------------------

btc() { "${BTC[@]}" "$@"; }

# Extract a top-level numeric JSON field without depending on jq.
json_num() { sed -n "s/.*\"$1\": *\([0-9][0-9.]*\).*/\1/p" | head -1; }

height() { btc getblockcount; }
mine() { btc generatetoaddress "$1" "$(btc getnewaddress)" > /dev/null; }

# Find the output paying `address` in a transaction: echoes "<vout> <sats>".
find_output() {
  btc getrawtransaction "$1" true | python3 -c '
import json, sys
address = sys.argv[1]
for out in json.load(sys.stdin)["vout"]:
    if out["scriptPubKey"].get("address") == address:
        print(out["n"], round(out["value"] * 100000000))
        break
' "$2"
}

# Fund the HTLC address and echo "<txid> <vout> <sats>".
fund_htlc() {
  local addr="$1" txid found
  txid=$(btc sendtoaddress "$addr" 1)
  mine 1
  found=$(find_output "$txid" "$addr")
  [ -n "$found" ] || { echo "no funding output for $addr in $txid" >&2; exit 1; }
  echo "$txid $found"
}

confirmations() { btc gettransaction "$1" | json_num confirmations; }

# --- claim path ------------------------------------------------------------

echo "== claim path =="
STATE="$WORK/claim.json"
LOCKTIME=$(( $(height) + 20 ))
ADDR=$("${SWAPX[@]}" init --state "$STATE" --locktime "$LOCKTIME" 2>/dev/null)
echo "htlc address:  $ADDR"
echo "refund height: $LOCKTIME"

read -r TXID VOUT SATS <<< "$(fund_htlc "$ADDR")"
echo "funded:        $TXID:$VOUT ($SATS sat)"

DEST=$(btc getnewaddress)
RAW=$("${SWAPX[@]}" claim --state "$STATE" \
  --outpoint "$TXID:$VOUT" --value "$SATS" --to "$DEST" --fee 2000 2>/dev/null)
CLAIM_TX=$(btc sendrawtransaction "$RAW")
mine 1

echo "claim tx:      $CLAIM_TX"
echo "confirmations: $(confirmations "$CLAIM_TX")"
[ "$(confirmations "$CLAIM_TX")" -ge 1 ] || { echo "claim did not confirm" >&2; exit 1; }

# --- refund path -----------------------------------------------------------

echo
echo "== refund path =="
STATE="$WORK/refund.json"
LOCKTIME=$(( $(height) + 12 ))
ADDR=$("${SWAPX[@]}" init --state "$STATE" --locktime "$LOCKTIME" 2>/dev/null)
echo "htlc address:  $ADDR"
echo "refund height: $LOCKTIME"

read -r TXID VOUT SATS <<< "$(fund_htlc "$ADDR")"
echo "funded:        $TXID:$VOUT ($SATS sat)"

DEST=$(btc getnewaddress)
RAW=$("${SWAPX[@]}" refund --state "$STATE" \
  --outpoint "$TXID:$VOUT" --value "$SATS" --to "$DEST" --fee 2000 2>/dev/null)

# The refund must be useless before the timeout opens.
if btc sendrawtransaction "$RAW" > /dev/null 2>&1; then
  echo "FAIL: refund accepted at height $(height), before locktime $LOCKTIME" >&2
  exit 1
fi
echo "early refund:  correctly rejected at height $(height)"

while [ "$(height)" -lt "$LOCKTIME" ]; do mine 1; done

REFUND_TX=$(btc sendrawtransaction "$RAW")
mine 1

echo "refund tx:     $REFUND_TX"
echo "confirmations: $(confirmations "$REFUND_TX")"
[ "$(confirmations "$REFUND_TX")" -ge 1 ] || { echo "refund did not confirm" >&2; exit 1; }

echo
echo "ok: claim and refund both confirmed on regtest"
