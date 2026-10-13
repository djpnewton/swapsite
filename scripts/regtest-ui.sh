#!/usr/bin/env bash
#
# The page, in a real browser, end to end.
#
# Every other script here drives HTTP or the wasm directly. This one drives the
# DOM. It covers the two payment blocks, the list's tabs, reporting a deposit by
# transaction id, and taking that deposit back after the locktime.
#
# Chrome is driven over the devtools protocol by web/e2e.mjs, which speaks both
# halves with what Node already has, so there is no driver to install.
#
# The maker is started with a huge tick interval on purpose. The refund flow
# needs an invoice the maker never pays and a deposit it therefore never claims,
# and a maker ticking once a second would pay it before the test got that far.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# On screen, for watching a run instead of reading its output.
show=""
case "${1:-}" in
  "") ;;
  --show | --headed) show=1 ;;
  *)
    echo "usage: $(basename "$0") [--show]" >&2
    exit 2
    ;;
esac

WASM="target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
# Ports of its own, so this can run while a dev stack is up.
RELAY_URL="http://127.0.0.1:3110"
MAKER_URL="http://127.0.0.1:3210"
COORD_URL="http://127.0.0.1:8093"
AMOUNT="${AMOUNT:-20000}"

SWAPS="$WORK/ui-maker-$$"
RECORDS="$WORK/ui-records-$$"
rm -rf "$SWAPS" "$RECORDS"
mkdir -p "$SWAPS"

cleanup() {
  # Each of these is allowed to find nothing: a non-zero pkill under `set -e`
  # would abandon the rest of the cleanup, and the exit status with it.
  pkill -f 'relay --listen 127.0.0.1:3110' 2>/dev/null || true
  pkill -f "maker ln2btc-serve --dir $SWAPS" 2>/dev/null || true
  pkill -f 'coordinator serve --listen 127.0.0.1:8093' 2>/dev/null || true
  return 0
}
trap cleanup EXIT

# The exit trap only knows this run's store, so a leftover from a run that was
# killed is cleared by the port it holds instead.
pkill -f 'relay --listen 127.0.0.1:3110' 2>/dev/null || true
pkill -f 'maker ln2btc-serve .*--listen 127.0.0.1:3210' 2>/dev/null || true
pkill -f 'coordinator serve --listen 127.0.0.1:8093' 2>/dev/null || true
sleep 0.5

# A browser is not something this project ships, and its absence is not a broken
# page: the test says so and passes, and `scripts/test.sh --regtest` stays usable
# on a machine without one.
if ! command -v "${CHROME:-google-chrome}" > /dev/null 2>&1; then
  echo "skipped: no ${CHROME:-google-chrome} on this machine" >&2
  exit 0
fi

echo "== build the module and start the services =="
cargo build --quiet --release --target wasm32-unknown-unknown -p swapsite-wasm

"$BIN/relay" --listen 127.0.0.1:3110 > "$WORK/ui-relay.out" 2>&1 &
"$BIN/maker" ln2btc-serve --dir "$SWAPS" --listen 127.0.0.1:3210 --interval 100000 \
  > "$WORK/ui-maker.out" 2>&1 &
"$BIN/coordinator" serve --listen 127.0.0.1:8093 \
  --maker-url "$MAKER_URL" --relay-url "$RELAY_URL" \
  --dir "$RECORDS" --web web --wasm "$WASM" \
  > "$WORK/ui-coord.out" 2>&1 &

for _ in $(seq 1 30); do
  curl -sf "$RELAY_URL/height" > /dev/null 2>&1 \
    && curl -sf "$MAKER_URL/swaps" > /dev/null 2>&1 \
    && curl -sf "$COORD_URL/config" > /dev/null 2>&1 && break
  sleep 0.5
done
echo "relay:         $RELAY_URL"
echo "maker:         $MAKER_URL (ticking every 100000s)"
echo "coordinator:   $COORD_URL"
echo "page:          $COORD_URL/"
[ -n "$show" ] && echo "browser:       on screen"

echo
echo "== the taker's invoice, and where a deposit should come back to =="
# The page reads the hash out of the invoice itself, so this only has to be a
# real one that the maker could have paid.
BOLT=$(docker compose -f docker/docker-compose.yml exec -T lnd-taker \
  lncli --network=regtest addinvoice --amt "$AMOUNT" --memo "ui test" \
  | grep -oE '"payment_request" *: *"[^"]*"' | head -1 | sed 's/.*: *"//;s/"$//')
[ -n "$BOLT" ] || fail "the taker's node produced no invoice"

REFUND=$(btc getnewaddress)
echo "invoice:       ${BOLT:0:24}... (${#BOLT} characters)"
echo "refund to:     $REFUND"

echo
echo "== the page in a browser =="
node web/e2e.mjs \
  --base="$COORD_URL" \
  --bitcoin="${BITCOIN_URL:-http://bitcoin:bitcoin@127.0.0.1:18443/wallet/swap}" \
  --invoice="$BOLT" \
  --refund-address="$REFUND" \
  ${show:+--headed} \
  ${CHROME:+--chrome="$CHROME"}
