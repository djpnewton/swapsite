# swapsite

Trustless swaps between BTC, Liquid (L-BTC) and Lightning.

The website is a **non-custodial coordinator**: it matches counterparties and
relays swap state, but every value transfer is enforced by hashlocked scripts
and Lightning HTLCs. The coordinator cannot move funds.

## Docs

- [docs/plan.md](docs/plan.md) - design, trust model, wallet requirements, roadmap
- [docs/regtest.md](docs/regtest.md) - local regtest environment (BTC + Lightning)
- [docs/m1.md](docs/m1.md) - protocol core: HTLC leg, timeouts, state machine
- [docs/m2.md](docs/m2.md) - coordinator, web UI, the client-side signer

## Status

M0 through M2 are done. A complete swap runs end to end on regtest: a maker
daemon that settles and refunds on its own, a taker that runs in WASM with no
node, and a web page that quotes the swap, pays the bond that starts it,
verifies the deposit itself, pays and claims. Next are the L-BTC legs.

Nothing is funded until the taker has paid a small non-refundable bond, which
covers the maker's entry and exit fees. Without it, abandoning a swap would be
free: the taker would risk nothing while the maker's capital sat in the HTLC for
the whole locktime.

```bash
cargo test                       # unit tests, includes real script verification
./scripts/format.sh              # rustfmt, then node --check and bash -n
./scripts/format.sh --check      # the same, changing nothing
./scripts/dev.sh                 # mine, lncli, bitcoin-cli, logs, records
docker compose -f docker/docker-compose.yml up -d
./scripts/regtest-init.sh        # fund nodes, open a balanced channel
./scripts/regtest-onchain.sh     # fund, claim and refund an HTLC on-chain
./scripts/regtest-ln2btc.sh      # one complete swap, both legs, plus the failure path
./scripts/regtest-btc2ln.sh      # the other direction: taker deposits first, no bond
./scripts/regtest-maker.sh       # the daemon: bond, cold-start settle and timed refund
./scripts/regtest-wasm.sh        # the browser-side taker, through a relay
./scripts/regtest-coordinator.sh # the whole flow: quote, verify, pay, claim, recover
./scripts/regtest-recovery.sh    # a lost page, a deadline, and a maker that reneges
```

To click through the same flow in a browser, start the three services and open
<http://127.0.0.1:8090/>:

```bash
target/debug/relay --listen 127.0.0.1:3100 &
target/debug/maker ln2btc-serve --dir /tmp/swaps --listen 127.0.0.1:3200 &
target/debug/coordinator serve --listen 127.0.0.1:8090 \
  --maker-url http://127.0.0.1:3200 --relay-url http://127.0.0.1:3100 \
  --dir /tmp/records --web web \
  --wasm target/wasm32-unknown-unknown/release/swapsite_wasm.wasm --dev-pay
```

## Running it in containers

One command builds everything and brings up the nodes with the relay, the maker,
the coordinator and a regtest control panel:

```bash
./scripts/docker-web.sh
```

- taker page: <http://127.0.0.1:8095/>
- control panel: <http://127.0.0.1:8091/> (mine blocks; heights, balances, channel
  capacity and swap states; and the taker's node as a wallet - its history, make an
  invoice, pay one, receive on-chain and send back out)
- relay: <http://127.0.0.1:3110/>

The services are behind a compose profile, so `docker compose up -d` still gives
you just the three nodes and the `regtest-*.sh` scripts keep working.

## Layout

| Crate | Contents |
| ----- | -------- |
| `core/` | the domain: HTLC, Lightning leg, swap state, maker decision logic. No I/O, so it compiles to WASM. |
| `adapters/` | the I/O: Bitcoin Core JSON-RPC, `lncli`, swap records on disk, one shared HTTP server and client. |
| `cli/` | `swapx` (drive one leg), `maker` (the daemon), `relay` (observe and broadcast), `coordinator` (quotes, brokering, static files), `regtestd` (a dev control panel). |
| `wasm/` | the taker, compiled to `wasm32-unknown-unknown` with no imports. |
| `web/` | the page, the control panel, and the JS that loads the module and talks to the relay. |
