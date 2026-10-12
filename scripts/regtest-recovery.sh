#!/usr/bin/env bash
#
# Recovery: what a taker can do after the page was closed.
#
# The failure this is about is a taker that pays and then loses the tab. The
# deposit is the taker's, but only a claim turns it into coins, and once the
# timeout passes the maker is entitled to take it back. Nothing can help a taker
# who never comes back. What this proves is that one who does come back can find
# out whether it still has a move, and that the warning is honest about the time
# left - including when the answer is that the swap is gone.

source "$(dirname "$0")/lib.sh"

AMOUNT="${AMOUNT:-30000}"
WASM="target/wasm32-unknown-unknown/release/swapsite_wasm.wasm"
RELAY_URL="http://127.0.0.1:3100"
MAKER_URL="http://127.0.0.1:3200"
COORD_URL="http://127.0.0.1:8090"

SWAPS="$WORK/recover-maker-$$"
RECORDS="$WORK/recover-records-$$"
rm -rf "$SWAPS" "$RECORDS"
mkdir -p "$SWAPS"

cleanup() {
  # Each of these is allowed to find nothing: a non-zero pkill under `set -e`
  # would abandon the rest of the cleanup, and the exit status with it.
  pkill -f 'relay --listen 127.0.0.1:3100' 2>/dev/null || true
  pkill -f "maker ln2btc-serve --dir $SWAPS" 2>/dev/null || true
  pkill -f 'coordinator serve --listen 127.0.0.1:8090' 2>/dev/null || true
  return 0
}
trap cleanup EXIT

pkill -f 'relay --listen 127.0.0.1:3100' 2>/dev/null || true
pkill -f 'maker ln2btc-serve --dir' 2>/dev/null || true
pkill -f 'coordinator serve --listen 127.0.0.1:8090' 2>/dev/null || true
sleep 0.5

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

# Two swaps, and a bond each, come out of the taker's node, so say so now rather
# than halfway through.
require_outbound "$((AMOUNT * 2 + 20000))" taker

echo "== build and start the three services =="
cargo build --quiet --release --target wasm32-unknown-unknown -p swapsite-wasm

"$BIN/relay" --listen 127.0.0.1:3100 > "$WORK/recovery-relay.out" 2>&1 &
"$BIN/maker" ln2btc-serve --dir "$SWAPS" --listen 127.0.0.1:3200 > "$WORK/recovery-maker.out" 2>&1 &
"$BIN/coordinator" serve --listen 127.0.0.1:8090 \
  --maker-url "$MAKER_URL" --relay-url "$RELAY_URL" \
  --dir "$RECORDS" --web web --wasm "$WASM" --dev-pay \
  > "$WORK/recovery-coord.out" 2>&1 &

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

# The taker's verdict on a swap. Neither the height nor the deposit's state is
# passed in: the CLI reads both from the chain, which is the only place they can
# come from.
verdict() {  # state, locktime, [preimage], [outpoint]
  local state="$1" locktime="$2" preimage="${3:-}" outpoint="${4:-}" args
  args=(--state "$state" --cltv-delta "$CLTV_DELTA" --locktime "$locktime")
  [ -n "$preimage" ] && args+=(--preimage "$preimage")
  [ -n "$outpoint" ] && args+=(--outpoint "$outpoint")
  taker assess "${args[@]}"
}

# The Lightning timeout the maker asks for. Replaced by the swap's own terms as
# soon as one is created, because the verdict needs it to judge the window.
CLTV_DELTA="${CLTV_DELTA:-40}"

# Buy a swap and pay for it, leaving the deposit unclaimed. This is where a
# taker is when the tab closes.
PAID_SWAP=""
make_and_pay() {  # echo "id locktime preimage outpoint sats"
  local swap id hash deposit locktime refund_pubkey verified vout sats preimage
  preimage=""
  swap=$(curl -s -X POST \
    -d "{\"amount\":$AMOUNT,\"claim_pubkey\":\"$TAKER_PUBKEY\"}" "$COORD_URL/swaps")
  id=$(printf '%s' "$swap" | jget .id)
  hash=$(printf '%s' "$swap" | jget .payment_hash)
  locktime=$(printf '%s' "$swap" | jget .locktime)
  refund_pubkey=$(printf '%s' "$swap" | jget .refund_pubkey)
  CLTV_DELTA=$(printf '%s' "$swap" | jget .cltv_delta)

  # The bond comes first: the maker funds nothing until it is in, and dev pay
  # stands in for the taker's wallet for both payments.
  paid=$(curl -s -X POST "$COORD_URL/dev/pay/$id" | jget .paid)
  [ "$paid" = "bond" ] || fail "dev pay did not pay the bond for $id"

  deposit=""
  for _ in $(seq 1 40); do
    deposit=$(curl -s "$COORD_URL/swaps/$id" | python3 -c '
import json, sys
print(json.load(sys.stdin).get("deposit_txid") or "")
')
    [ -n "$deposit" ] && break
    sleep 0.5
  done
  [ -n "$deposit" ] || fail "the maker never funded $id after its bond"

  mine 1
  verified=$(taker verify-deposit --payment-hash "$hash" --locktime "$locktime" \
    --claim-pubkey "$TAKER_PUBKEY" --refund-pubkey "$refund_pubkey" --txid "$deposit")
  vout=$(printf '%s\n' "$verified" | field vout)
  sats=$(printf '%s\n' "$verified" | field sats)

  preimage=$(curl -s -X POST "$COORD_URL/dev/pay/$id" | jget .preimage)
  [ -n "$preimage" ] || fail "no preimage came back for $id"

  PAID_SWAP="$id"
  echo "$id $locktime $preimage $deposit:$vout $sats"
}

echo
echo "== the page's key is made in the browser =="
KEY=$(taker keypair)
TAKER_SECRET=$(printf '%s\n' "$KEY" | field secret)
TAKER_PUBKEY=$(printf '%s\n' "$KEY" | field pubkey)
echo "pubkey:        ${TAKER_PUBKEY:0:24}..."

# --- part one: the taker comes back in time --------------------------------

echo
echo "== a taker pays for a swap and then loses the page =="
read -r ID LOCKTIME PREIMAGE OUTPOINT SATS <<< "$(make_and_pay)"
echo "swap id:       $ID"
echo "paid:          $AMOUNT sat over Lightning, deposit $SATS sat unclaimed"

# Everything a reloaded page has: the key it made, the preimage its wallet gave
# it, and whatever the coordinator remembers. No deposit, no outpoint, no swap
# parameters of its own.
echo
echo "== the tab is closed. Only the key and the preimage survive =="
BACK=$(curl -s "$COORD_URL/swaps?claim_pubkey=$TAKER_PUBKEY")
FOUND=$(printf '%s' "$BACK" | jget ".swaps.0.id")
[ "$FOUND" = "$ID" ] || fail "the coordinator did not remember $ID (got $FOUND)"
STATE=$(printf '%s' "$BACK" | jget ".swaps.0.state")
echo "found again:   $ID (state $STATE)"

V=$(verdict "$STATE" "$LOCKTIME" "$PREIMAGE" "$OUTPOINT")
ACTION=$(field action <<< "$V")
LEFT=$(field blocks_left <<< "$V")
[ "$ACTION" = "claim" ] || fail "expected a claim verdict, got $ACTION"
[ "$LEFT" -gt 0 ] || fail "the verdict left no room to claim ($LEFT)"
echo "verdict:       $ACTION, $LEFT blocks left (urgent $(field urgent <<< "$V"))"

# Claim it the way a returned page would: rebuild the deposit check from chain
# data, then sign. Nothing here needs the old tab.
echo
echo "== the taker claims what it never claimed =="
VERIFIED=$(taker verify-deposit --payment-hash "$(curl -s "$COORD_URL/swaps/$ID" | jget .payment_hash)" \
  --locktime "$LOCKTIME" --claim-pubkey "$TAKER_PUBKEY" \
  --refund-pubkey "$(curl -s "$COORD_URL/swaps/$ID" | jget .refund_pubkey)" \
  --txid "$(curl -s "$COORD_URL/swaps/$ID" | jget .deposit_txid)")
[ "${OUTPOINT##*:}" = "$(printf '%s\n' "$VERIFIED" | field vout)" ] || fail "the deposit moved"

DEST=$(btc getnewaddress)
CLAIM=$(taker claim --claim-key "$TAKER_SECRET" \
  --payment-hash "$(curl -s "$COORD_URL/swaps/$ID" | jget .payment_hash)" \
  --locktime "$LOCKTIME" \
  --refund-pubkey "$(curl -s "$COORD_URL/swaps/$ID" | jget .refund_pubkey)" \
  --preimage "$PREIMAGE" --outpoint "$OUTPOINT" \
  --value "$SATS" --to "$DEST" --fee "$(curl -s "$COORD_URL/swaps/$ID" | jget .claim_fee_sat)")
CLAIM_TX=$(taker broadcast --tx-hex "$(printf '%s\n' "$CLAIM" | field tx_hex)" | field txid)
mine 1

read -r _ RECEIVED <<< "$(find_output "$CLAIM_TX" "$DEST")"
[ "$RECEIVED" = "$AMOUNT" ] || fail "recovered $RECEIVED, expected $AMOUNT"
echo "recovered:     $CLAIM_TX ($RECEIVED sat, after the tab was gone)"

# --- part two: the deadline, and what it is actually for --------------------
#
# The daemon never refunds a swap the taker paid for: `plan` returns Done the
# moment the invoice settles. The script does not care. The refund path is the
# maker's key behind a CLTV, and nothing on chain consults the daemon's opinion,
# so a maker that reneges can take a deposit it has already been paid for. That
# is the whole reason the deadline matters, so this part makes the maker do it.

echo
echo "== a second swap is paid for and then left alone =="
read -r ID2 LOCKTIME2 PREIMAGE2 OUTPOINT2 SATS2 <<< "$(make_and_pay)"
echo "swap id:       $ID2"
echo "refund opens:  height $LOCKTIME2 (now $(height))"

# The maker's own key, straight off its record. A real reneging maker would just
# use it; here it is on disk, so the test can borrow it.
REFUND_SECRET=$(python3 -c '
import json, sys
print(json.load(open(sys.argv[1]))["refund_secret"])
' "$SWAPS/$ID2.json")

jswap() { curl -s "$COORD_URL/swaps/$ID2" | jget "$1"; }

# The refund the maker would send. swapx writes its diagnostics to stderr and the
# raw transaction to stdout, so this captures only the transaction.
build_refund() {
  swapx refund --payment-hash "$(jswap .payment_hash)" \
    --locktime "$LOCKTIME2" --claim-pubkey "$(jswap .claim_pubkey)" \
    --refund-key "$REFUND_SECRET" --outpoint "$OUTPOINT2" \
    --value "$SATS2" --to "$(btc getnewaddress)" \
    --fee "$(jswap .claim_fee_sat)"
}

V=$(verdict settled "$LOCKTIME2" "$PREIMAGE2" "$OUTPOINT2")
ACTION=$(field action <<< "$V")
[ "$(field urgent <<< "$V")" = "false" ] || fail "urgent far too early"
[ "$(field deposit_unspent <<< "$V")" = "true" ] || fail "the deposit is already gone"
echo "verdict:       $ACTION, $(field blocks_left <<< "$V") blocks left, not urgent"

echo
echo "== before the deadline the maker cannot take it, however it feels =="
EARLY=$(build_refund 2> /dev/null)
[ -n "$EARLY" ] || fail "swapx produced no refund to try"

# The refusal has to be the timelock. An empty or malformed transaction would be
# refused as well, and would make this look like a pass for the wrong reason.
REASON=$(btc sendrawtransaction "$EARLY" 2>&1) || true
printf '%s' "$REASON" | grep -qi 'non-final\|nonfinal' \
  || fail "expected the timelock to refuse the refund, got: $REASON"
echo "refund:        refused as non-final at height $(height), $((LOCKTIME2 - $(height))) blocks early"

V=$(verdict settled "$LOCKTIME2" "$PREIMAGE2" "$OUTPOINT2")
[ "$(field action <<< "$V")" = "$ACTION" ] || fail "the verdict changed without the chain"

# Walk to the point where the warning should be loud, and stop there.
echo
echo "== the warning goes off a margin before the deadline =="
WARN_AT=12
mine "$((LOCKTIME2 - $(height) - WARN_AT))"
V=$(verdict settled "$LOCKTIME2" "$PREIMAGE2" "$OUTPOINT2")
ACTION=$(field action <<< "$V")
LEFT=$(field blocks_left <<< "$V")
[ "$ACTION" = "claim" ] || fail "expected claim at $WARN_AT blocks, got $ACTION"
[ "$LEFT" = "$WARN_AT" ] || fail "expected $WARN_AT blocks left, got $LEFT"
[ "$(field urgent <<< "$V")" = "true" ] || fail "not urgent with $LEFT blocks left"
echo "verdict:       $ACTION, $LEFT blocks left, urgent"

# The deadline itself. The refund path opens at that height, not after it.
echo
echo "== the deadline arrives with the deposit still unclaimed =="
mine "$WARN_AT"
V=$(verdict settled "$LOCKTIME2" "$PREIMAGE2" "$OUTPOINT2")
ACTION=$(field action <<< "$V")
[ "$ACTION" = "racing" ] || fail "expected racing at the deadline, got $ACTION"
[ "$(field blocks_since <<< "$V")" = "0" ] || fail "expected the deadline itself"
[ "$(field deposit_unspent <<< "$V")" = "true" ] || fail "the deposit went too early"
echo "height:        $(height), refund path open"
echo "verdict:       $ACTION - a claim still wins, but only if it gets there first"

echo
echo "== the maker takes a deposit it was already paid for =="
REFUND_TX=$(btc sendrawtransaction "$(build_refund 2> /dev/null)")
mine 1
echo "refund tx:     $REFUND_TX"

# Nothing on the maker's own side knows about this: it only ever followed the
# refund it built itself, so its record still says settled. The chain is the only
# thing that tells the taker, which is why the verdict has to ask it.
STATE2=$(jswap .state)
echo "maker's view:  $STATE2 (it cannot see this refund)"
[ "$STATE2" = "settled" ] || fail "expected the maker to be none the wiser, got $STATE2"

V=$(verdict "$STATE2" "$LOCKTIME2" "$PREIMAGE2" "$OUTPOINT2")
ACTION=$(field action <<< "$V")
[ "$(field deposit_unspent <<< "$V")" = "false" ] || fail "the spent deposit was not noticed"
[ "$ACTION" = "gone" ] || fail "expected gone, got $ACTION"
echo "verdict:       $ACTION"

# The verdict has to describe reality, so prove the ending is real rather than a
# label: the claim the taker would have built can no longer be mined.
echo
echo "== the claim the taker could have made is now worthless =="
CLAIM2=$(taker claim --claim-key "$TAKER_SECRET" \
  --payment-hash "$(jswap .payment_hash)" --locktime "$LOCKTIME2" \
  --refund-pubkey "$(jswap .refund_pubkey)" \
  --preimage "$PREIMAGE2" --outpoint "$OUTPOINT2" \
  --value "$SATS2" --to "$(btc getnewaddress)" --fee "$(jswap .claim_fee_sat)")

if taker broadcast --tx-hex "$(printf '%s\n' "$CLAIM2" | field tx_hex)" > /dev/null 2>&1; then
  fail "a claim against a refunded deposit was accepted"
fi
echo "broadcast:     refused, because the output it spends is gone"

# --- a window too short to pay into ---------------------------------------
#
# The taker's own check, and the one that does not depend on the maker having
# configured anything correctly: a payment made now expires cltv_delta blocks
# from here, and the refund has to open after that plus a margin.

echo
echo "== a window too short to pay into is refused =="
NOW=$(height)
SHORT=$(taker assess --state initiator_funded --cltv-delta 40 \
  --locktime "$(( NOW + 51 ))" --height "$NOW")
[ "$(field action <<< "$SHORT")" = "too_late_to_pay" ] \
  || fail "expected too_late_to_pay, got $(field action <<< "$SHORT")"
echo "51 blocks:     $(field action <<< "$SHORT") - a 40 block payment needs 52"

# The same window once it has closed: nothing to count down to any more.
CLOSED=$(taker assess --state initiator_funded --cltv-delta 40 \
  --locktime "$NOW" --height "$NOW")
[ "$(field action <<< "$CLOSED")" = "expired" ] \
  || fail "expected expired, got $(field action <<< "$CLOSED")"
echo "0 blocks:      $(field action <<< "$CLOSED") - the refund path is open"

ROOMY=$(taker assess --state initiator_funded --cltv-delta 40 \
  --locktime "$(( NOW + 52 ))" --height "$NOW")
[ "$(field action <<< "$ROOMY")" = "wait_for_payment" ] \
  || fail "expected wait_for_payment at the floor, got $(field action <<< "$ROOMY")"
echo "52 blocks:     $(field action <<< "$ROOMY") - the floor is enough"

echo
echo "ok: a lost page recovered its swap, and a deposit taken after payment was reported"
