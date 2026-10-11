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

M0 and M1a are done. The on-chain HTLC leg is implemented in Rust, verified with
`libbitcoinconsensus` and end to end on regtest. Next is the Lightning leg.

```bash
cargo test                       # unit tests, includes real script verification
docker compose -f docker/docker-compose.yml up -d
./scripts/regtest-init.sh        # fund nodes, open a channel
./scripts/regtest-onchain.sh     # fund, claim and refund an HTLC on-chain
```
