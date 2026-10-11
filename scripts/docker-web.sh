#!/usr/bin/env bash
#
# The whole web scenario, in containers.
#
# Brings up the three nodes plus the relay, the maker, the coordinator and the
# control panel, funds the network if it is new, and prints the two URLs.
#
#   taker page    http://127.0.0.1:8095/
#   control panel http://127.0.0.1:8091/
#
# The regtest-*.sh scripts drive binaries on the host and want ports 3100/3200/
# 8090/8091, so stop this before running those:
#
#   docker compose --profile web down

source "$(dirname "$0")/lib.sh"

WEB=(docker compose -f docker/docker-compose.yml --profile web)
TAKER_URL="http://127.0.0.1:8095/"
PANEL_URL="http://127.0.0.1:8091/"

echo "== building and starting =="
"${WEB[@]}" up -d --build --wait --wait-timeout 180

# A fresh volume has three running nodes and no channel, and a swap needs one.
# Cheap to check, and much cheaper than debugging a payment that fails for a
# reason the page cannot explain.
#
# Wait for the channel to come up before concluding there is not one: a restarted
# node answers `getinfo` before it has reconnected to its peer, and a channel in
# that state reads exactly like no channel at all. Deciding early funds the
# network a second time, which is what happened the first time this ran.
for _ in $(seq 1 60); do
  [ "$(ln_outbound_sat taker)" -ge 100000 ] && break
  sleep 1
done

if [ "$(ln_outbound_sat taker)" -lt 100000 ]; then
  echo
  echo "== no usable channel: funding the network =="
  ./scripts/regtest-init.sh
fi

echo
echo "== waiting for the services to answer =="
for _ in $(seq 1 60); do
  curl -sf "http://127.0.0.1:8095/config" > /dev/null 2>&1 \
    && curl -sf "http://127.0.0.1:8091/status" > /dev/null 2>&1 && break
  sleep 1
done
curl -sf "http://127.0.0.1:8095/config" > /dev/null 2>&1 \
  || fail "the coordinator never answered (docker compose --profile web logs coordinator)"
curl -sf "http://127.0.0.1:8091/status" > /dev/null 2>&1 \
  || fail "the control panel never answered (docker compose --profile web logs regtestd)"

echo
echo "taker page:    $TAKER_URL"
echo "control panel: $PANEL_URL"
echo
echo "the panel mines the block a swap needs; the relay the page talks to is on"
echo "127.0.0.1:3110, which is why the coordinator hands the browser that address."
echo
echo "stop it with:  ${WEB[*]} down"
