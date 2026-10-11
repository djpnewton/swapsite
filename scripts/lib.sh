#!/usr/bin/env bash
#
# Shared helpers for the regtest scripts. Source this, do not execute it.
#
# Requires: the docker compose stack up (docs/regtest.md), cargo, python3.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

COMPOSE=(docker compose -f docker/docker-compose.yml)
BTC=("${COMPOSE[@]}" exec -T bitcoind bitcoin-cli -regtest \
  -rpcuser=bitcoin -rpcpassword=bitcoin -rpcwallet=swap)
SWAPX=(cargo run --quiet --bin swapx --)

WORK="${TMPDIR:-/tmp}/swapsite-regtest"
mkdir -p "$WORK"

btc() { "${BTC[@]}" "$@"; }
swapx() { "${SWAPX[@]}" "$@"; }

# Extract `key=value` from swapx's key=value output.
field() { sed -n "s/^$1=//p" | head -1; }

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

# Fund an HTLC address with `sats` and echo "<txid> <vout> <sats>".
fund_htlc() {
  local addr="$1" sats="$2" txid found
  txid=$(btc sendtoaddress "$addr" "$(to_btc "$sats")")
  mine 1
  found=$(find_output "$txid" "$addr")
  [ -n "$found" ] || { echo "no funding output for $addr in $txid" >&2; exit 1; }
  echo "$txid $found"
}

to_btc() { awk -v s="$1" 'BEGIN { printf "%.8f", s / 100000000 }'; }

confirmations() { btc gettransaction "$1" | json_num confirmations; }

# The node's spendable outbound Lightning capacity, in satoshis.
#
# `local_balance` is not spendable in full: every channel holds a reserve back.
ln_outbound_sat() {
  local node="${1:-taker}"
  docker compose -f docker/docker-compose.yml \
    exec -T "lnd-$node" lncli --network=regtest listchannels \
    | python3 -c '
import json, sys
channels = json.load(sys.stdin).get("channels", [])
spendable = sum(
    int(c["local_balance"]) - int(c.get("local_chan_reserve_sat") or 0)
    for c in channels
    if c.get("active")
)
print(max(spendable, 0))
'
}

require_outbound() {
  local needed="$1" node="${2:-taker}" available
  available=$(ln_outbound_sat "$node")
  [ -n "$available" ] || fail "could not read $node's Lightning balance"
  [ "$needed" -le "$available" ] \
    || fail "$node has $available sat spendable, needs $needed. Reset the network with: docker compose -f docker/docker-compose.yml down -v && ./scripts/regtest-init.sh"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Poll the maker's view of an invoice until it reaches `wanted`.
await_invoice_state() {
  local hash="$1" wanted="$2" node="${3:-maker}" state=""
  for _ in $(seq 1 30); do
    state=$(swapx ln state --node "$node" --payment-hash "$hash" 2>/dev/null \
      | field state) || true
    [ "$state" = "$wanted" ] && { echo "$state"; return 0; }
    sleep 1
  done
  echo "$state"
  return 1
}
