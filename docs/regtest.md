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

To run the whole web flow over HTTP - quote, verify the deposit, pay, claim, and
find the swap again by claim key:

```bash
./scripts/regtest-coordinator.sh
```

To run recovery: a taker that loses the page and comes back, a deadline counted
down to the block, and a maker that refunds a deposit it was already paid for:

```bash
./scripts/regtest-recovery.sh
```

All six scripts source `scripts/lib.sh`, which builds the binaries once and
needs `cargo`, `python3` and a running compose stack; `regtest-wasm.sh`,
`regtest-coordinator.sh` and `regtest-recovery.sh` also need `node` and the
`wasm32-unknown-unknown` target (`rustup target add wasm32-unknown-unknown`).
The daemon and service scripts kill anything left over from an earlier run
before they start.

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
- Both nodes run as root and take their data directory from `/etc/passwd`, not
  from `$HOME`: bitcoind goes to `/root/.bitcoin` and LND to `/root/.lnd`. The
  volumes are mounted at those paths. Mounting them where the images' READMEs
  suggest leaves an empty volume and a chain that dies with the container, which
  is what this file used to do - `down` and `down -v` were indistinguishable.
- The LND image's entrypoint creates a user and drops privileges. Anything built
  on that image has to set its own `entrypoint` to skip it, or it cannot read the
  admin macaroon.

## The whole scenario in containers

`./scripts/docker-web.sh` builds one image and runs the three nodes plus the four
services. It funds the network if it is new, waits for the page to answer, and
prints the two URLs:

| What          | Where                    | Serves                                  |
| ------------- | ------------------------ | --------------------------------------- |
| taker page    | <http://127.0.0.1:8095/> | the page a user swaps from               |
| control panel | <http://127.0.0.1:8091/> | height, mining, balances, swap states    |
| relay         | <http://127.0.0.1:3110/> | what the browser reads the chain through |

The services are behind a compose **profile**, so `docker compose up -d` still
brings up only the three nodes and the `regtest-*.sh` scripts keep working. They
want ports 3100/3200/8090, which is why the containerised ones use 3110/8095/8091
and the two can run at once.

```bash
./scripts/docker-web.sh                                   # build, start, report
docker compose -f docker/docker-compose.yml --profile web logs -f coordinator
docker compose -f docker/docker-compose.yml --profile web down      # keeps state
docker compose -f docker/docker-compose.yml --profile web down -v   # wipes it
```

### The control panel

`regtestd` is a separate binary on purpose: it mines blocks and reads node
wallets, and the coordinator must never be able to do either. It is the fastest
way to drive a swap by hand, because the alternative is shelling into a container
to mine a block.

```text
GET  /status           height, balances, per-node capacity, the maker's swaps
POST /mine             {"blocks": N}, capped at 200
POST /taker/invoice    {"sats": N, "memo": "..."}  -> {"bolt11"}
POST /taker/pay        {"bolt11": "..."}           -> {"preimage"}
POST /taker/address    {}                          -> {"address"}
POST /taker/send       {"address": "...", "sats": N} -> {"txid"}
```

The two `address`/`send` routes are absent from `/status` on purpose. `lncli
newaddress` returns a **fresh** address every time it is called, so an address
behind a two-second poll would be a new one every two seconds. The address is
handed out when asked and the request is the only thing that asks.

It reports each section independently, so a dead node shows an error in its own
card rather than blanking the page. Its node cards turn red when a node has no
spendable outbound: a channel can be up and still unable to pay, and the payment
failure says nothing about why.

The taker's node gets a wallet view: its on-chain balance, its channel capacity,
both directions of its history (sent payments and received invoices, merged and
newest first), and the two things a wallet does - make an invoice, pay one. The
**On chain** section adds the other half: get an address to send coins to, or
send coins out. Lightning alone is not enough to drive the node here, because
funding a channel and paying a swap's deposit are both on-chain moves. That is
the dev stand-in for the user's wallet, and it is what makes the real payment
route testable without `--dev-pay`.

### Paying a swap from the panel

This is the flow the invoice display exists for, and it is worth walking once
because it exercises four things at the same time:

1. On the taker page, **Get a quote**. The card shows the **bond**: pay it before
   `bond_by`, because nothing is funded until it is in.
2. Paste the bond into the panel and **Pay**. It is a plain invoice, not a hold
   one, so that payment settles at once - nothing is held, and the maker funds on
   its next tick. The card then shows the deposit and the swap's own invoice.
3. Paste that invoice into the panel and **Pay**. This one stays open until the
   maker settles, and it should: `lncli payinvoice` waits for the payment to
   reach a terminal state, and a held payment has not reached one - it is a
   promise, not a settlement. The panel serves each connection on its own thread,
   so it keeps answering, which is what makes step 4 possible at all.
4. **Mine a block** in the panel. The maker settles on its next tick, the payment
   completes, and the swap reads `settled`.
5. Nothing else. The page has been saying `find_preimage` - it has no preimage,
   because it never paid - and by the next poll the maker has settled and
   published one, so the verdict turns into `claim`. Name an address and it
   claims; with an address already on the card it claims by itself.

Step 5 is worth being precise about, because it is the one place the two sides
have to agree. Settling *is* the reveal: a hold invoice is settled by handing
over the preimage, which is what tells the Lightning network the payment is
claimable, so the preimage is public from that moment. A maker that withheld it
would not be protecting anything - it would only be costing a taker who paid from
another wallet a manual copy-paste. So `PublicSwap` carries the preimage on a
**settled** swap and on nothing else, and the field is absent rather than null
before then, so a swap's public view cannot be read as "the secret is out".

Step 2 is the bond, and it is why the deposit does not exist when the page first
shows the swap. The maker will not fund until it has been paid: funding first
would make this whole flow free to walk away from at step 3, since the taker
would risk nothing while the maker's capital sat in the HTLC for the locktime.
The bond is priced at cost - the entry and exit fees - so the maker recovers them
and gains nothing.

The copy-paste route stays as the fallback. A maker that is unreachable, or one
running an older build, leaves the page at `find_preimage`, and then the wallet
that paid is the only source of the preimage; the page says so and keeps the
inline field.

### How the services reach Lightning

On a host, `lncli` is exec'd inside the compose container, which needs docker and
nothing else. In a container there is no docker, so the services run `lncli`
directly against the node's gRPC port, naming the credentials rather than relying
on a home directory to find them:

```bash
maker serve --lnd-rpc lnd-maker:10009 --lnd-dir /lnd-maker
```

`--lnd-dir` holds `tls.cert` and `data/chain/bitcoin/<network>/admin.macaroon`.
The node's volume is mounted read-only at that path, because writing to a node's
data directory is not the maker's business. Setting one of the two without the
other is an error rather than a fallback, because the fallback fails with a
message about docker that has nothing to do with the mistake.

Reaching a node by name needs the certificate to vouch for the name, so both
nodes run with `--tlsextradomain=lnd-maker` / `=lnd-taker`.

### Building it

The image is one Rust build stage and one runtime stage. Two things about it are
worth knowing before changing it:

- The runtime base is the node's own image, not a slim Debian, because three of
  the four services shell out to `lncli` and that binary does not survive being
  copied across distributions.
- The wasm build needs `clang`: `cc-rs` compiles secp256k1's C sources for
  `wasm32-unknown-unknown`, and without it the failure surfaces from inside a
  dependency as `cc-rs: failed to find tool "clang"`.
