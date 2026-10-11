# swapsite

Trustless swaps between BTC, Liquid (L-BTC) and Lightning.

The website is a **non-custodial coordinator**: it matches counterparties and
relays swap state, but every value transfer is enforced by hashlocked scripts
and Lightning HTLCs. The coordinator cannot move funds.

## Docs

- [docs/plan.md](docs/plan.md) - design, trust model, wallet requirements, roadmap
- [docs/regtest.md](docs/regtest.md) - local regtest environment (BTC + Lightning)
- [docs/m1.md](docs/m1.md) - protocol core: HTLC leg, timeouts, state machine

## Status

M0 through M1d are done. A complete swap runs end to end on regtest: a maker
daemon that settles and refunds on its own, and a taker that runs in WASM with
no node, verifying the deposit itself and signing its own claim. Next is the
coordinator and the web UI.

```bash
cargo test                       # unit tests, includes real script verification
docker compose -f docker/docker-compose.yml up -d
./scripts/regtest-init.sh        # fund nodes, open a balanced channel
./scripts/regtest-onchain.sh     # fund, claim and refund an HTLC on-chain
./scripts/regtest-swap.sh        # one complete swap, both legs, plus the failure path
./scripts/regtest-maker.sh       # the daemon: cold-start settle and timed refund
./scripts/regtest-wasm.sh        # the browser-side taker, through a relay
```

## Layout

| Crate | Contents |
| ----- | -------- |
| `core/` | the domain: HTLC, Lightning leg, swap state, maker decision logic. No I/O, so it compiles to WASM. |
| `adapters/` | the I/O: Bitcoin Core JSON-RPC, `lncli`, swap records on disk. |
| `cli/` | `swapx` (drive one leg), `maker` (the daemon), `relay` (observe and broadcast). |
| `wasm/` | the taker, compiled to `wasm32-unknown-unknown` with no imports. |
| `web/` | the JS that loads the module and talks to the relay. |
