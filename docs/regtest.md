# Local regtest environment

A Docker Compose network for testing swaps against real Bitcoin Core and real
Lightning nodes on regtest.

Scope for M0: **BTC + Lightning only**. Liquid (`elementsd`) arrives in M3.

## Topology

| Service     | Image                            | Purpose                           | Host ports                       |
| ----------- | -------------------------------- | --------------------------------- | -------------------------------- |
| `bitcoind`  | `polarlightning/bitcoind:27.0`   | regtest chain, RPC + ZMQ          | 18443 (rpc), 18444 (p2p)         |
| `lnd-maker` | `polarlightning/lnd:0.18.3-beta` | the maker's node, holds a channel | 10009 (rpc), 18080 (rest), 9735  |
| `lnd-taker` | `polarlightning/lnd:0.18.3-beta` | stands in for the user's LN wallet | 10010 (rpc), 18081 (rest), 9736  |

Host REST ports are `18080` / `18081` because `8080` is commonly already taken
on a dev machine. Inside the compose network the REST ports stay `8080`.

RPC credentials are `bitcoin` / `bitcoin`. LND runs with `--noseedbackup`, so
there is no wallet password and no unlock step.

Image tags are pinned but may need bumping; the compose file is a scaffold.

Verified working: 101-block chain, both nodes funded, channel active
(996,530 sat on the maker side), maker -> taker payment settled with the
preimage revealed, and `addholdinvoice` accepted (state `OPEN`).

## Usage

```bash
docker compose -f docker/docker-compose.yml up -d
./scripts/regtest-init.sh
```

`regtest-init.sh` waits for the nodes, creates a wallet, mines 101 blocks (for
coinbase maturity), funds both LND nodes, connects them, opens a channel from
the maker to the taker and confirms it. Half the channel is pushed to the taker,
so both nodes have spendable outbound capacity and a swap can be tested in
either direction.

To exercise an on-chain HTLC leg against this chain (fund, claim, and a refund
that is rejected early and then accepted after the timeout):

```bash
./scripts/regtest-onchain.sh
```

To run one complete swap, both legs, plus the canceled-invoice failure path:

```bash
./scripts/regtest-swap.sh
```

To run the maker daemon end to end (a cold-start settle, and a timed refund):

```bash
./scripts/regtest-maker.sh
```

To run the browser-side taker, which uses WASM for everything cryptographic and
a relay only to observe and broadcast:

```bash
./scripts/regtest-wasm.sh
```

All four scripts source `scripts/lib.sh`, which builds the binaries once and
needs `cargo` and `python3`; `regtest-wasm.sh` also needs `node` and the
`wasm32-unknown-unknown` target. `regtest-maker.sh` and `regtest-wasm.sh` kill
any daemon or relay left over from an earlier run before they start.

Each swap moves real balance from the taker to the maker over Lightning, so a
long series of runs drains the taker's outbound capacity. The scripts refuse to
start if capacity is too low and say so; reset with:

```bash
docker compose -f docker/docker-compose.yml down -v
./scripts/regtest-init.sh
```

Tear down, keeping data:

```bash
docker compose -f docker/docker-compose.yml down
```

Tear down and wipe the chain:

```bash
docker compose -f docker/docker-compose.yml down -v
```

## Poking at it by hand

Bitcoin:

```bash
docker compose -f docker/docker-compose.yml exec bitcoind \
  bitcoin-cli -regtest -rpcuser=bitcoin -rpcpassword=bitcoin \
  -rpcwallet=swap getblockcount
```

Mine a block:

```bash
docker compose -f docker/docker-compose.yml exec bitcoind \
  bitcoin-cli -regtest -rpcuser=bitcoin -rpcpassword=bitcoin \
  -rpcwallet=swap getnewaddress | \
  xargs -I{} docker compose -f docker/docker-compose.yml exec -T bitcoind \
  bitcoin-cli -regtest -rpcuser=bitcoin -rpcpassword=bitcoin \
  -rpcwallet=swap generatetoaddress 1 {}
```

Lightning:

```bash
docker compose -f docker/docker-compose.yml exec lnd-maker \
  lncli --network=regtest getinfo

docker compose -f docker/docker-compose.yml exec lnd-taker \
  lncli --network=regtest addinvoice --amt=100000
```

A HODL invoice (what the maker issues) needs the preimage up front:

```bash
docker compose -f docker/docker-compose.yml exec lnd-maker \
  lncli --network=regtest addholdinvoice <hex-preimage> --amt=100000
```

## Connecting the coordinator

From inside the compose network:

- Maker LND REST / gRPC: `https://lnd-maker:8080`, gRPC `lnd-maker:10009`
- TLS cert: `/home/lnd/.lnd/tls.cert`
- Admin macaroon:
  `/home/lnd/.lnd/data/chain/bitcoin/regtest/admin.macaroon`
- Bitcoin RPC: `http://bitcoind:18443` with `bitcoin` / `bitcoin`

The coordinator holds no keys. It needs LND access only to observe invoices and
to have the maker act; it never needs the taker's keys.

## Clicking through a swap in a browser

Three processes, then the page:

```bash
target/debug/relay --listen 127.0.0.1:3100 &
target/debug/maker serve --dir /tmp/swaps --listen 127.0.0.1:3200 &
target/debug/coordinator serve --listen 127.0.0.1:8090 \
  --maker-url http://127.0.0.1:3200 --relay-url http://127.0.0.1:3100 \
  --dir /tmp/records --web web \
  --wasm target/wasm32-unknown-unknown/release/swapsite_wasm.wasm --dev-pay
```

Build the module first: `./scripts/regtest-wasm.sh` does it, or
`cargo build -p swapsite-wasm --release --target wasm32-unknown-unknown`.

Then <http://127.0.0.1:8090/>: quote, mine a block, check the deposit, pay,
claim, mine again. The maker funds the deposit on its own, so the page needs no
wallet until the claim, which it signs itself. `--dev-pay` pays the taker's
invoice from the taker's own node so the flow can be driven without a second
wallet; without it the page shows an invoice for the user to pay.

Close the tab after paying and reopen it before the deposit's locktime: the page
finds the swap, counts the blocks it has left, and claims it without being asked
as long as it remembers a destination address. Mine up to the locktime instead
and the warning turns into a race.

`scripts/regtest-coordinator.sh` and `scripts/regtest-recovery.sh` run the same
flows over HTTP, from the shell. Do not run them while these three are up: they
want the same ports.

## The relay's endpoints

```text
GET  /height              {"height": N}
GET  /tx/<txid>           {"txid", "confirmations", "hex"}
GET  /utxo/<txid>:<vout>  {"unspent": bool, "sats": N|null}
POST /tx                  {"txid"}   body is raw transaction hex
```

`/utxo` is how a taker notices its deposit has been taken without believing
anyone: the maker's own records cannot see a refund the maker did not build.

## Notes and gotchas

- Named volumes inherit ownership from the image path, so the non-root users in
  the images can write. If permissions bite on your Docker setup, add
  `user: "1000:1000"` to the offending service.
- `--noseedbackup` means **no seed is ever shown**. That is fine for regtest and
  catastrophic anywhere else. Do not reuse these confs on mainnet.
- ZMQ is wired up because LND uses it for faster block notification; the
  coordinator can also poll `getblockchaininfo`.
- A browser-facing Esplora / electrs service is not included yet. The relay
  covers what the taker needs - height, a transaction, broadcast - but it is not
  a light client and it does not serve the wallet-side queries (utxos, history)
  that M3's chain-backed signer would want.
- Killing a daemon needs the flags, not the path: the binaries run from
  `target/debug`, so `pkill -f "bin/relay"` matches nothing and leaves the
  daemon alive to race the next run on the same port and store. Match something
  distinctive such as `relay --listen 127.0.0.1:3100`.
- A `set -e` trap that cleans up processes needs `|| true` on each `pkill`, or
  the trap aborts with a non-zero status of its own.
- The coordinator and frontend are still absent from the compose file: both take
  host paths (`--web`, `--wasm`, `--dir`) that only make sense from the
  repository root.
