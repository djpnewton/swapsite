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

All three scripts source `scripts/lib.sh`, which builds the binaries once and
needs `cargo` and `python3`. `regtest-maker.sh` kills any daemon left over from
an earlier run before it starts.

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

## Notes and gotchas

- Named volumes inherit ownership from the image path, so the non-root users in
  the images can write. If permissions bite on your Docker setup, add
  `user: "1000:1000"` to the offending service.
- `--noseedbackup` means **no seed is ever shown**. That is fine for regtest and
  catastrophic anywhere else. Do not reuse these confs on mainnet.
- ZMQ is wired up because LND uses it for faster block notification; the
  coordinator can also poll `getblockchaininfo`.
- A browser-facing Esplora / electrs service is not included yet. It is needed
  once the frontend has to observe the chain itself (M2).
- The coordinator and frontend services are intentionally absent from the
  compose file until there is code to run.
