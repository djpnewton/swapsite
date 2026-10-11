#!/usr/bin/env bash
#
# Bootstrap the regtest swap environment: wallets, funds, peers, channel.
# Idempotent enough to re-run; safe to run after `up -d`.

set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -f docker/docker-compose.yml)
BTC=("${COMPOSE[@]}" exec -T bitcoind bitcoin-cli -regtest -rpcuser=bitcoin -rpcpassword=bitcoin)
LND_MAKER=("${COMPOSE[@]}" exec -T lnd-maker lncli --network=regtest)
LND_TAKER=("${COMPOSE[@]}" exec -T lnd-taker lncli --network=regtest)

CHANNEL_AMT=5000000  # 0.05 BTC, enough headroom for repeated swap tests

# Pull a top-level string field out of lncli's JSON without depending on jq.
json_field() {
  sed -n "s/.*\"$1\": *\"\([^\"]*\)\".*/\1/p" | head -n 1
}

wait_for() {
  local label="$1"
  shift
  printf 'waiting for %s' "$label"
  for _ in $(seq 1 60); do
    if "$@" >/dev/null 2>&1; then
      printf ' ok\n'
      return 0
    fi
    printf '.'
    sleep 1
  done
  printf ' timed out\n'
  return 1
}

wait_for bitcoind "${BTC[@]}" getblockchaininfo
wait_for lnd-maker "${LND_MAKER[@]}" getinfo
wait_for lnd-taker "${LND_TAKER[@]}" getinfo

# Wallet + initial chain.
"${BTC[@]}" createwallet swap >/dev/null 2>&1 \
  || "${BTC[@]}" loadwallet swap >/dev/null 2>&1 \
  || true

ADDR="$("${BTC[@]}" -rpcwallet=swap getnewaddress)"
HEIGHT="$("${BTC[@]}" -rpcwallet=swap getblockcount)"

if [ "$HEIGHT" -lt 101 ]; then
  echo "mining to height 101"
  "${BTC[@]}" -rpcwallet=swap generatetoaddress "$((101 - HEIGHT))" "$ADDR" >/dev/null
fi

# Fund both nodes. 6 confirmations so the funds are spendable for channels.
for node in LND_MAKER LND_TAKER; do
  declare -n ref="$node"
  node_addr="$("${ref[@]}" newaddress p2wkh | json_field address)"
  echo "funding $node at $node_addr"
  "${BTC[@]}" -rpcwallet=swap sendtoaddress "$node_addr" 1 >/dev/null
done

"${BTC[@]}" -rpcwallet=swap generatetoaddress 6 "$ADDR" >/dev/null

# Peer + channel.
MAKER_PUB="$("${LND_MAKER[@]}" getinfo | json_field identity_pubkey)"
TAKER_PUB="$("${LND_TAKER[@]}" getinfo | json_field identity_pubkey)"

if ! "${LND_TAKER[@]}" listpeers | grep -q "$MAKER_PUB"; then
  echo "connecting maker -> taker"
  "${LND_MAKER[@]}" connect "${TAKER_PUB}@lnd-taker:9735"
fi

if [ "$("${LND_MAKER[@]}" listchannels | grep -c '"active": true' || true)" -eq 0 ]; then
  echo "opening channel maker -> taker ($CHANNEL_AMT sat, half pushed to the taker)"
  # Pushing half the balance to the taker gives both nodes spendable outbound
  # capacity, so a swap can be tested in either direction.
  "${LND_MAKER[@]}" openchannel --node_key="$TAKER_PUB" \
    --local_amt="$CHANNEL_AMT" --push_amt="$((CHANNEL_AMT / 2))"
  "${BTC[@]}" -rpcwallet=swap generatetoaddress 6 "$ADDR" >/dev/null
fi

echo
echo "maker local balance:"
"${LND_MAKER[@]}" channelbalance
echo "taker local balance:"
"${LND_TAKER[@]}" channelbalance
echo
echo "ready. see docs/regtest.md for how to poke at it."
