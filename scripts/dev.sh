#!/usr/bin/env bash
#
# common dev commands
#
# The stack has to be up: ./scripts/docker-web.sh, or
# `docker compose -f docker/docker-compose.yml up -d`.

source "$(dirname "$0")/lib.sh"

usage() {
  cat <<'EOF'
dev.sh - the commands that come up while poking at a running stack

  mine [n]                     mine n blocks, default 1, and print the height
  height                       the chain height
  btc <bitcoin-cli ...>        bitcoin-cli inside the bitcoind container
  ln <maker|taker> <...>       lncli inside that Lightning node
  exec <service> <cmd...>      run anything in any container
  logs <service> [n]           the last n log lines, default 30
  records [maker|coordinator]  the swap records that exist
  swap <id or prefix>          one maker record, as JSON

Services: bitcoind, lnd-maker, lnd-taker, relay, maker, coordinator, regtestd.
EOF
}

case "${1:-}" in
  mine)
    mine "${2:-1}"
    echo "height $(height)"
    ;;
  height) height ;;
  btc)
    shift
    btc "$@"
    ;;
  ln)
    [ -n "${2:-}" ] || {
      usage >&2
      exit 2
    }
    node="$2"
    shift 2
    "${COMPOSE[@]}" exec -T "lnd-$node" lncli --network=regtest "$@"
    ;;
  exec)
    [ -n "${2:-}" ] || {
      usage >&2
      exit 2
    }
    service="$2"
    shift 2
    "${COMPOSE[@]}" exec -T "$service" "$@"
    ;;
  logs)
    [ -n "${2:-}" ] || {
      usage >&2
      exit 2
    }
    "${COMPOSE[@]}" logs --tail="${3:-30}" "$2"
    ;;
  records)
    if [ "${2:-maker}" = coordinator ]; then
      "${COMPOSE[@]}" exec -T coordinator ls /records
    else
      "${COMPOSE[@]}" exec -T maker ls /swaps
    fi
    ;;
  swap)
    [ -n "${2:-}" ] || {
      usage >&2
      exit 2
    }
    prefix="$2"
    # The ids are 64 characters and nobody types those, so a prefix is what
    # anyone has. An ambiguous one is refused rather than guessed at.
    matches=$("${COMPOSE[@]}" exec -T maker sh -c 'ls /swaps' | grep "^$prefix" || true)
    count=$(printf '%s\n' "$matches" | grep -c . || true)
    [ "$count" -gt 0 ] || fail "no maker record starts with $prefix"
    if [ "$count" -gt 1 ]; then
      printf '%s\n' "$matches" >&2
      fail "that prefix matches more than one record"
    fi
    "${COMPOSE[@]}" exec -T maker cat "/swaps/$matches" | python3 -m json.tool
    ;;
  "" | help | -h | --help) usage ;;
  *)
    echo "unknown command: $1" >&2
    usage >&2
    exit 2
    ;;
esac
